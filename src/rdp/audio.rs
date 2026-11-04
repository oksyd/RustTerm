//! Bounded, cancellable PulseAudio playback on a dedicated event-driven worker.

use super::RdpError;
use rustrdp::{AudioEvent, AudioFormat, AudioSamples, AudioToken};
use std::{
    collections::VecDeque,
    ffi::{c_char, c_int, c_ulong, c_void},
    io::{Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MAX_BYTES: usize = 1024 * 1024;
// Leave room below the session's 64 unacknowledged blocks for the in-flight
// block and confirmations awaiting the protocol loop. A slow audio device must
// shed excess samples before it exhausts protocol capacity.
const MAX_EVENTS: usize = 32;
const COMPLETION_CAPACITY: usize = 65;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

pub(super) enum Completion {
    Consumed { token: AudioToken, elapsed_ms: u32 },
    Unavailable,
}

pub(super) struct Playback {
    shared: Arc<Shared>,
    wake: UnixStream,
    completed: mpsc::Sender<Completion>,
    pub(super) completions: mpsc::Receiver<Completion>,
    volume: [u16; 2],
}

struct Shared {
    queue: Mutex<Queue>,
    stopped: Arc<AtomicBool>,
    failed: AtomicBool,
    muted: Arc<AtomicBool>,
}

#[derive(Default)]
struct Queue {
    items: VecDeque<Item>,
    bytes: usize,
}

enum Item {
    Samples(Chunk),
    Close,
}

struct Chunk {
    samples: AudioSamples,
    received: Instant,
    volume: [u16; 2],
}

impl Playback {
    pub(super) fn new(muted: Arc<AtomicBool>) -> Result<Self, RdpError> {
        let (wake, reader) = UnixStream::pair().map_err(|_| RdpError::AudioUnavailable)?;
        wake.set_nonblocking(true)
            .map_err(|_| RdpError::AudioUnavailable)?;
        reader
            .set_nonblocking(true)
            .map_err(|_| RdpError::AudioUnavailable)?;
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            stopped: Arc::new(AtomicBool::new(false)),
            failed: AtomicBool::new(false),
            muted,
        });
        // The session retains at most 64 unacknowledged blocks. One additional
        // slot carries a device failure without blocking the audio thread.
        let (completed, completions) = mpsc::channel(COMPLETION_CAPACITY);
        let worker_shared = shared.clone();
        let worker_completed = completed.clone();
        std::thread::Builder::new()
            .name("rustterm-rdp-audio".into())
            .spawn(move || run(worker_shared, reader, worker_completed))
            .map_err(|_| RdpError::AudioUnavailable)?;
        Ok(Self {
            shared,
            wake,
            completed,
            completions,
            volume: [u16::MAX; 2],
        })
    }

    pub(super) fn push(&mut self, event: AudioEvent) -> Result<(), RdpError> {
        let item = match event {
            AudioEvent::Volume { left, right } => {
                self.volume = [left, right];
                return Ok(());
            }
            AudioEvent::Samples(samples) => Item::Samples(Chunk {
                samples,
                received: Instant::now(),
                volume: self.volume,
            }),
            AudioEvent::Close => Item::Close,
        };
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        let closing = matches!(&item, Item::Close);
        if closing && matches!(queue.items.back(), Some(Item::Close)) {
            return Ok(());
        }
        let length = match &item {
            Item::Samples(chunk) => chunk.samples.data.len(),
            Item::Close => 0,
        };
        if self.shared.failed.load(Ordering::Acquire)
            || self.shared.stopped.load(Ordering::Acquire)
            || queue.items.len() >= MAX_EVENTS + usize::from(closing)
            || length > MAX_BYTES.saturating_sub(queue.bytes)
        {
            drop(queue);
            if let Item::Samples(chunk) = item {
                self.completed
                    .try_send(completion(chunk))
                    .map_err(|_| RdpError::AudioUnavailable)?;
            }
            return Ok(());
        }
        queue.bytes += length;
        queue.items.push_back(item);
        drop(queue);
        wake(&self.wake);
        Ok(())
    }

    pub(super) fn refresh_mute(&self) {
        wake(&self.wake);
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        wake(&self.wake);
    }
}

fn wake(stream: &UnixStream) {
    // A full socket already represents a pending wakeup.
    let _ = (&*stream).write(&[1]);
}

fn completion(chunk: Chunk) -> Completion {
    let elapsed_ms = chunk
        .received
        .elapsed()
        .as_millis()
        .min(u128::from(u32::MAX)) as u32;
    Completion::Consumed {
        token: chunk.samples.token,
        elapsed_ms,
    }
}

fn run(shared: Arc<Shared>, reader: UnixStream, completed: mpsc::Sender<Completion>) {
    let mut poll = Box::new(PollState {
        reader,
        stopped: shared.stopped.clone(),
        deadline: None,
        fds: Vec::new(),
    });
    let mut pulse = None;
    while !shared.stopped.load(Ordering::Acquire) {
        let item = shared
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .items
            .pop_front();
        let result = match item {
            Some(Item::Samples(mut chunk)) => {
                let result = if shared.muted.load(Ordering::Acquire) {
                    Ok(())
                } else {
                    if pulse.is_none() {
                        pulse = Pulse::connect(&mut poll).ok();
                    }
                    match &mut pulse {
                        Some(pulse) => pulse.play(&mut chunk, &mut poll, &shared.muted),
                        None => Err(()),
                    }
                };
                let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                queue.bytes -= chunk.samples.data.len();
                drop(queue);
                // Only this worker may wait for completion capacity. Dropping
                // Playback drops the receiver and immediately releases the wait.
                if completed.blocking_send(completion(chunk)).is_err() {
                    break;
                }
                result
            }
            Some(Item::Close) => match &mut pulse {
                Some(pulse) => pulse.close_stream(&mut poll),
                None => Ok(()),
            },
            None => match &mut pulse {
                Some(pulse) => pulse.iterate(&mut poll, None),
                None => poll.wait(),
            },
        };
        if result.is_err() && !shared.stopped.load(Ordering::Acquire) {
            shared.failed.store(true, Ordering::Release);
            if completed.blocking_send(Completion::Unavailable).is_err() {
                break;
            }
            // Keep the RDP session live: consume already queued blocks, then
            // let push() acknowledge subsequent blocks without retrying a
            // failed sound server for every packet.
            let pending = {
                let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                queue.bytes = 0;
                std::mem::take(&mut queue.items)
            };
            for item in pending {
                if let Item::Samples(chunk) = item
                    && completed.blocking_send(completion(chunk)).is_err()
                {
                    break;
                }
            }
            break;
        }
    }
    // Pulse is dropped before its callback userdata and before unloading libpulse.
    drop(pulse);
}

struct PollState {
    reader: UnixStream,
    stopped: Arc<AtomicBool>,
    deadline: Option<Instant>,
    fds: Vec<libc::pollfd>,
}

impl PollState {
    fn wait(&mut self) -> Result<(), ()> {
        // No libpulse context has been opened. Wait for the first audio block.
        let result =
            unsafe { poll_with_wakeup(ptr::null_mut(), 0, -1, ptr::from_mut(self).cast()) };
        if result < 0 { Err(()) } else { Ok(()) }
    }
}

unsafe extern "C" fn poll_with_wakeup(
    fds: *mut libc::pollfd,
    count: c_ulong,
    timeout: c_int,
    userdata: *mut c_void,
) -> c_int {
    // libpulse invokes this synchronously on its sole owning worker. The boxed
    // userdata outlives the mainloop and all descriptors belong to that loop.
    let state = unsafe { &mut *userdata.cast::<PollState>() };
    if count > 4096 {
        return -1;
    }
    if state.stopped.load(Ordering::Acquire) {
        return 0;
    }
    let original = if count == 0 {
        &mut []
    } else {
        unsafe { std::slice::from_raw_parts_mut(fds, count as usize) }
    };
    state.fds.clear();
    state.fds.extend_from_slice(original);
    state.fds.push(libc::pollfd {
        fd: state.reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    });
    let timeout = match state.deadline {
        Some(deadline) => {
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(i32::MAX as u128) as i32;
            if timeout < 0 {
                remaining
            } else {
                timeout.min(remaining)
            }
        }
        None => timeout,
    };
    let result = unsafe {
        libc::poll(
            state.fds.as_mut_ptr(),
            state.fds.len() as libc::nfds_t,
            timeout,
        )
    };
    if result < 0 {
        return if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            0
        } else {
            -1
        };
    }
    let mut ready = 0;
    for (target, source) in original.iter_mut().zip(&state.fds) {
        target.revents = source.revents;
        ready += c_int::from(source.revents != 0);
    }
    if state.fds[count as usize].revents != 0 {
        let mut bytes = [0; 128];
        while state.reader.read(&mut bytes).is_ok_and(|read| read != 0) {}
    }
    ready
}

type Poll = unsafe extern "C" fn(*mut libc::pollfd, c_ulong, c_int, *mut c_void) -> c_int;
type Success = unsafe extern "C" fn(*mut c_void, c_int, *mut c_void);
type Request = unsafe extern "C" fn(*mut c_void, usize, *mut c_void);
type Free = unsafe extern "C" fn(*mut c_void);

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

macro_rules! api {
    ($($name:ident: $signature:ty),+ $(,)?) => {
        struct Api { $($name: $signature,)+ _library: libloading::Library }
        impl Api {
            fn load() -> Result<Self, ()> {
                // Only ABI-stable libpulse symbols are resolved. The handle is
                // retained until every context, stream and mainloop is freed.
                unsafe {
                    let library = libloading::Library::new("libpulse.so.0").map_err(|_| ())?;
                    $(let $name = *library.get::<$signature>(concat!(stringify!($name), "\0").as_bytes()).map_err(|_| ())?;)+
                    Ok(Self { $($name,)+ _library: library })
                }
            }
        }
    }
}

api! {
    pa_mainloop_new: unsafe extern "C" fn() -> *mut c_void,
    pa_mainloop_free: unsafe extern "C" fn(*mut c_void),
    pa_mainloop_get_api: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    pa_mainloop_set_poll_func: unsafe extern "C" fn(*mut c_void, Option<Poll>, *mut c_void),
    pa_mainloop_iterate: unsafe extern "C" fn(*mut c_void, c_int, *mut c_int) -> c_int,
    pa_context_new: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    pa_context_connect: unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *const c_void) -> c_int,
    pa_context_get_state: unsafe extern "C" fn(*const c_void) -> c_int,
    pa_context_disconnect: unsafe extern "C" fn(*mut c_void),
    pa_context_unref: unsafe extern "C" fn(*mut c_void),
    pa_stream_new: unsafe extern "C" fn(*mut c_void, *const c_char, *const SampleSpec, *const c_void) -> *mut c_void,
    pa_stream_connect_record: unsafe extern "C" fn(*mut c_void, *const c_char, *const BufferAttr, c_int) -> c_int,
    pa_stream_set_read_callback: unsafe extern "C" fn(*mut c_void, Option<Request>, *mut c_void),
    pa_stream_peek: unsafe extern "C" fn(*mut c_void, *mut *const c_void, *mut usize) -> c_int,
    pa_stream_drop: unsafe extern "C" fn(*mut c_void) -> c_int,
    pa_stream_connect_playback: unsafe extern "C" fn(*mut c_void, *const c_char, *const BufferAttr, c_int, *const c_void, *mut c_void) -> c_int,
    pa_stream_set_write_callback: unsafe extern "C" fn(*mut c_void, Option<Request>, *mut c_void),
    pa_stream_get_state: unsafe extern "C" fn(*const c_void) -> c_int,
    pa_stream_writable_size: unsafe extern "C" fn(*const c_void) -> usize,
    pa_stream_write: unsafe extern "C" fn(*mut c_void, *const c_void, usize, Option<Free>, i64, c_int) -> c_int,
    pa_stream_drain: unsafe extern "C" fn(*mut c_void, Option<Success>, *mut c_void) -> *mut c_void,
    pa_stream_disconnect: unsafe extern "C" fn(*mut c_void) -> c_int,
    pa_stream_unref: unsafe extern "C" fn(*mut c_void),
    pa_operation_get_state: unsafe extern "C" fn(*const c_void) -> c_int,
    pa_operation_cancel: unsafe extern "C" fn(*mut c_void),
    pa_operation_unref: unsafe extern "C" fn(*mut c_void),
}

struct Pulse {
    api: Api,
    mainloop: *mut c_void,
    context: *mut c_void,
    stream: *mut c_void,
    format: Option<AudioFormat>,
}

impl Pulse {
    fn connect(poll: &mut PollState) -> Result<Self, ()> {
        let api = Api::load()?;
        let mut pulse = Self {
            api,
            mainloop: ptr::null_mut(),
            context: ptr::null_mut(),
            stream: ptr::null_mut(),
            format: None,
        };
        unsafe {
            pulse.mainloop = (pulse.api.pa_mainloop_new)();
            if pulse.mainloop.is_null() {
                return Err(());
            }
            pulse.context = (pulse.api.pa_context_new)(
                (pulse.api.pa_mainloop_get_api)(pulse.mainloop),
                c"RustTerm remote audio".as_ptr(),
            );
            if pulse.context.is_null()
                || (pulse.api.pa_context_connect)(pulse.context, ptr::null(), 1, ptr::null()) < 0
            {
                return Err(());
            }
            let deadline = Instant::now() + CONNECT_TIMEOUT;
            loop {
                match (pulse.api.pa_context_get_state)(pulse.context) {
                    4 => return Ok(pulse),
                    5 | 6 => return Err(()),
                    _ => pulse.iterate(poll, Some(deadline))?,
                }
            }
        }
    }

    fn iterate(&mut self, poll: &mut PollState, deadline: Option<Instant>) -> Result<(), ()> {
        if poll.stopped.load(Ordering::Acquire)
            || deadline.is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(());
        }
        poll.deadline = deadline;
        let result = unsafe {
            // Rust lends poll exclusively to the callback for this iteration.
            (self.api.pa_mainloop_set_poll_func)(
                self.mainloop,
                Some(poll_with_wakeup),
                ptr::from_mut(poll).cast(),
            );
            (self.api.pa_mainloop_iterate)(self.mainloop, 1, ptr::null_mut())
        };
        poll.deadline = None;
        if result < 0 || unsafe { (self.api.pa_context_get_state)(self.context) } >= 5 {
            Err(())
        } else {
            Ok(())
        }
    }

    fn play(
        &mut self,
        chunk: &mut Chunk,
        poll: &mut PollState,
        muted: &AtomicBool,
    ) -> Result<(), ()> {
        let format = chunk.samples.format;
        if self.format != Some(format) {
            self.close_stream(poll)?;
            self.open_stream(format, poll)?;
        }
        apply_volume(&mut chunk.samples.data, format.channels, chunk.volume);
        let alignment = usize::from(format.channels) * 2;
        let mut offset = 0;
        while offset < chunk.samples.data.len() {
            if poll.stopped.load(Ordering::Acquire) {
                return Err(());
            }
            if muted.load(Ordering::Acquire) {
                return Ok(());
            }
            if unsafe { (self.api.pa_stream_get_state)(self.stream) } != 2 {
                return Err(());
            }
            let writable = unsafe { (self.api.pa_stream_writable_size)(self.stream) };
            if writable == usize::MAX {
                return Err(());
            }
            let length = writable.min(chunk.samples.data.len() - offset) / alignment * alignment;
            if length == 0 {
                self.iterate(poll, None)?;
                continue;
            }
            let result = unsafe {
                (self.api.pa_stream_write)(
                    self.stream,
                    chunk.samples.data[offset..].as_ptr().cast(),
                    length,
                    None,
                    0,
                    0,
                )
            };
            if result < 0 {
                return Err(());
            }
            offset += length;
        }
        Ok(())
    }

    fn open_stream(&mut self, format: AudioFormat, poll: &mut PollState) -> Result<(), ()> {
        let spec = SampleSpec {
            format: 3,
            rate: format.sample_rate,
            channels: format.channels as u8,
        };
        let frame_bytes = u32::from(format.channels) * 2;
        let buffer = BufferAttr {
            maxlength: format.sample_rate / 5 * frame_bytes,
            tlength: format.sample_rate / 10 * frame_bytes,
            prebuf: 0,
            minreq: format.sample_rate / 100 * frame_bytes,
            fragsize: u32::MAX,
        };
        unsafe {
            self.stream = (self.api.pa_stream_new)(
                self.context,
                c"Remote desktop".as_ptr(),
                &spec,
                ptr::null(),
            );
            if self.stream.is_null() {
                return Err(());
            }
            (self.api.pa_stream_set_write_callback)(self.stream, Some(writable), ptr::null_mut());
            if (self.api.pa_stream_connect_playback)(
                self.stream,
                ptr::null(),
                &buffer,
                0x2000,
                ptr::null(),
                ptr::null_mut(),
            ) < 0
            {
                return Err(());
            }
            let deadline = Instant::now() + CONNECT_TIMEOUT;
            loop {
                match (self.api.pa_stream_get_state)(self.stream) {
                    2 => {
                        self.format = Some(format);
                        return Ok(());
                    }
                    3 | 4 => return Err(()),
                    _ => self.iterate(poll, Some(deadline))?,
                }
            }
        }
    }

    fn open_capture(&mut self, format: AudioFormat, poll: &mut PollState) -> Result<(), ()> {
        let spec = SampleSpec {
            format: 3,
            rate: format.sample_rate,
            channels: format.channels as u8,
        };
        let bytes_per_second = format.sample_rate * u32::from(format.channels) * 2;
        let buffer = BufferAttr {
            maxlength: bytes_per_second / 2,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: bytes_per_second / 50,
        };
        unsafe {
            self.stream = (self.api.pa_stream_new)(
                self.context,
                c"RustTerm microphone".as_ptr(),
                &spec,
                ptr::null(),
            );
            if self.stream.is_null() {
                return Err(());
            }
            (self.api.pa_stream_set_read_callback)(self.stream, Some(writable), ptr::null_mut());
            if (self.api.pa_stream_connect_record)(self.stream, ptr::null(), &buffer, 0x2000) < 0 {
                return Err(());
            }
            let deadline = Instant::now() + CONNECT_TIMEOUT;
            loop {
                match (self.api.pa_stream_get_state)(self.stream) {
                    2 => {
                        self.format = Some(format);
                        return Ok(());
                    }
                    3 | 4 => return Err(()),
                    _ => self.iterate(poll, Some(deadline))?,
                }
            }
        }
    }
    fn capture(&mut self, bytes: &mut Vec<u8>, poll: &mut PollState) -> Result<(), ()> {
        let mut data = ptr::null();
        let mut length = 0;
        unsafe {
            if (self.api.pa_stream_get_state)(self.stream) != 2
                || (self.api.pa_stream_peek)(self.stream, &mut data, &mut length) < 0
            {
                return Err(());
            }
            if length == 0 {
                return self.iterate(poll, None);
            }
            if length > MAX_BYTES
                || bytes
                    .len()
                    .checked_add(length)
                    .is_none_or(|n| n > MAX_BYTES)
            {
                return Err(());
            }
            if data.is_null() {
                bytes.resize(bytes.len() + length, 0);
            } else {
                bytes.extend_from_slice(std::slice::from_raw_parts(data.cast::<u8>(), length));
            }
            if (self.api.pa_stream_drop)(self.stream) < 0 {
                return Err(());
            }
        }
        Ok(())
    }

    fn close_stream(&mut self, poll: &mut PollState) -> Result<(), ()> {
        if self.stream.is_null() {
            return Ok(());
        }
        let operation = unsafe { (self.api.pa_stream_drain)(self.stream, None, ptr::null_mut()) };
        if operation.is_null() {
            return Err(());
        }
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let result = loop {
            if unsafe { (self.api.pa_operation_get_state)(operation) } != 0 {
                break Ok(());
            }
            if self.iterate(poll, Some(deadline)).is_err() {
                break Err(());
            }
        };
        unsafe {
            if result.is_err() {
                (self.api.pa_operation_cancel)(operation);
            }
            (self.api.pa_operation_unref)(operation);
        }
        self.disconnect_stream();
        result
    }

    fn disconnect_stream(&mut self) {
        if !self.stream.is_null() {
            unsafe {
                (self.api.pa_stream_set_write_callback)(self.stream, None, ptr::null_mut());
                (self.api.pa_stream_set_read_callback)(self.stream, None, ptr::null_mut());
                (self.api.pa_stream_disconnect)(self.stream);
                (self.api.pa_stream_unref)(self.stream);
            }
            self.stream = ptr::null_mut();
            self.format = None;
        }
    }
}

impl Drop for Pulse {
    fn drop(&mut self) {
        self.disconnect_stream();
        unsafe {
            if !self.context.is_null() {
                (self.api.pa_context_disconnect)(self.context);
                (self.api.pa_context_unref)(self.context);
            }
            if !self.mainloop.is_null() {
                (self.api.pa_mainloop_free)(self.mainloop);
            }
        }
    }
}

unsafe extern "C" fn writable(_: *mut c_void, _: usize, _: *mut c_void) {}

fn apply_volume(data: &mut [u8], channels: u16, volume: [u16; 2]) {
    if volume == [u16::MAX; 2] {
        return;
    }
    for (index, bytes) in data.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        let channel = if channels == 1 { 0 } else { index % 2 };
        let sample = i32::from(i16::from_le_bytes([bytes[0], bytes[1]]));
        let scaled = sample * i32::from(volume[channel]) / i32::from(u16::MAX);
        bytes.copy_from_slice(&(scaled as i16).to_le_bytes());
    }
}

pub(super) enum Captured {
    Opened {
        generation: u64,
        available: bool,
    },
    Samples {
        generation: u64,
        data: Vec<u8>,
        transmission: CancellationToken,
        discontinuity: bool,
    },
    Failed,
}
pub(super) struct Capture {
    stopped: Arc<AtomicBool>,
    wake: UnixStream,
    pub events: mpsc::Receiver<Captured>,
}
impl Capture {
    pub fn new(
        request: rustrdp::CaptureRequest,
        allowed: Arc<AtomicBool>,
        locked: Arc<AtomicBool>,
        transmission: Arc<Mutex<CancellationToken>>,
    ) -> Result<Self, RdpError> {
        let (wake_socket, reader) =
            UnixStream::pair().map_err(|_| RdpError::MicrophoneUnavailable)?;
        wake_socket
            .set_nonblocking(true)
            .map_err(|_| RdpError::MicrophoneUnavailable)?;
        reader
            .set_nonblocking(true)
            .map_err(|_| RdpError::MicrophoneUnavailable)?;
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let (tx, events) = mpsc::channel(8);
        std::thread::Builder::new()
            .name("rustterm-rdp-microphone".into())
            .spawn(move || {
                let mut poll = PollState {
                    reader,
                    stopped: stop.clone(),
                    deadline: None,
                    fds: Vec::new(),
                };
                let mut opened = false;
                let mut discontinuity = true;
                let mut pulse: Option<Pulse> = None;
                let mut bytes = Vec::new();
                let mut buffered_epoch = transmission
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                let packet_bytes =
                    request.frames as usize * usize::from(request.format.channels) * 2;
                if packet_bytes == 0 || packet_bytes > 256 * 1024 {
                    let _ = tx.blocking_send(Captured::Opened {
                        generation: request.generation,
                        available: false,
                    });
                    return;
                }
                let result = (|| -> Result<(), ()> {
                    while !stop.load(Ordering::Acquire) {
                        if locked.load(Ordering::Acquire) || !allowed.load(Ordering::Acquire) {
                            if let Some(pulse) = &mut pulse {
                                pulse.disconnect_stream();
                            }
                            bytes.clear();
                            discontinuity = true;
                            poll.wait()?;
                            continue;
                        }
                        if pulse.is_none() {
                            pulse = Some(Pulse::connect(&mut poll)?);
                        }
                        let Some(pulse) = &mut pulse else {
                            return Err(());
                        };
                        if pulse.stream.is_null() {
                            pulse.open_capture(request.format, &mut poll)?;
                        }
                        if !opened {
                            tx.blocking_send(Captured::Opened {
                                generation: request.generation,
                                available: true,
                            })
                            .map_err(|_| ())?;
                            opened = true;
                        }
                        if buffered_epoch.is_cancelled() {
                            bytes.clear();
                            discontinuity = true;
                        }
                        let epoch = transmission
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .clone();
                        buffered_epoch = epoch.clone();
                        pulse.capture(&mut bytes, &mut poll)?;
                        if epoch.is_cancelled()
                            || locked.load(Ordering::Acquire)
                            || !allowed.load(Ordering::Acquire)
                        {
                            bytes.clear();
                            discontinuity = true;
                            continue;
                        }
                        while bytes.len() >= packet_bytes {
                            let remaining = bytes.split_off(packet_bytes);
                            let data = std::mem::replace(&mut bytes, remaining);
                            match tx.try_send(Captured::Samples {
                                generation: request.generation,
                                data,
                                transmission: epoch.clone(),
                                discontinuity,
                            }) {
                                Ok(()) => discontinuity = false,
                                Err(mpsc::error::TrySendError::Full(_)) => discontinuity = true,
                                Err(mpsc::error::TrySendError::Closed(_)) => return Err(()),
                            }
                        }
                    }
                    Ok(())
                })();
                if result.is_err() && !stop.load(Ordering::Acquire) {
                    let event = if opened {
                        Captured::Failed
                    } else {
                        Captured::Opened {
                            generation: request.generation,
                            available: false,
                        }
                    };
                    let _ = tx.blocking_send(event);
                }
            })
            .map_err(|_| RdpError::Runtime)?;
        Ok(Self {
            stopped,
            wake: wake_socket,
            events,
        })
    }
    pub fn refresh(&self) {
        wake(&self.wake);
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        wake(&self.wake);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_volume_preserves_channel_order_and_signed_extremes() {
        let mut bytes: Vec<_> = [i16::MIN, i16::MAX, 1000, -1000]
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect();
        apply_volume(&mut bytes, 2, [u16::MAX, 0]);
        let values: Vec<_> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(values, [i16::MIN, 0, 1000, 0]);
    }

    #[test]
    fn mono_volume_does_not_alternate_between_left_and_right_settings() {
        let mut bytes: Vec<_> = [1000i16, -1000]
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect();
        apply_volume(&mut bytes, 1, [0, u16::MAX]);
        assert_eq!(bytes, [0; 4]);
    }
}
