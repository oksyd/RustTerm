use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub struct Wire<S> {
    pub stream: S,
    pub mysql: bool,
    sequence: u8,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Wire<S> {
    pub fn new(stream: S, mysql: bool) -> Self {
        Self {
            stream,
            mysql,
            sequence: 0,
        }
    }

    pub async fn packet(&mut self, body: &[u8]) {
        let mut header = (body.len() as u32).to_le_bytes();
        header[3] = self.sequence;
        self.sequence += 1;
        self.stream.write_all(&header).await.unwrap();
        self.stream.write_all(body).await.unwrap();
        self.stream.flush().await.unwrap();
    }

    pub async fn read_packet(&mut self) -> Vec<u8> {
        let mut header = [0; 4];
        self.stream.read_exact(&mut header).await.unwrap();
        self.sequence = header[3] + 1;
        header[3] = 0;
        let mut body = vec![0; u32::from_le_bytes(header) as usize];
        self.stream.read_exact(&mut body).await.unwrap();
        body
    }

    pub async fn message(&mut self, tag: u8, body: &[u8]) {
        self.stream.write_u8(tag).await.unwrap();
        self.stream.write_u32(body.len() as u32 + 4).await.unwrap();
        self.stream.write_all(body).await.unwrap();
        self.stream.flush().await.unwrap();
    }

    pub async fn greeting(&mut self, tls: bool) {
        let caps: u32 = 1
            | 2
            | 4
            | 8
            | 0x200
            | 0x2000
            | 0x8000
            | 0x10000
            | 0x20000
            | 0x80000
            | if tls { 0x800 } else { 0 };
        let mut hello = b"\x0a8.0.36\0".to_vec();
        hello.extend(1u32.to_le_bytes());
        hello.extend(b"12345678\0");
        hello.extend((caps as u16).to_le_bytes());
        hello.extend([33, 2, 0]);
        hello.extend(((caps >> 16) as u16).to_le_bytes());
        hello.push(21);
        hello.extend([0; 10]);
        hello.extend(b"901234567890\0mysql_native_password\0");
        self.packet(&hello).await;
    }

    pub async fn login(&mut self) {
        if self.mysql {
            self.read_packet().await;
            self.complete(0, false).await;
        } else {
            let len = self.stream.read_u32().await.unwrap();
            let mut startup = vec![0; len as usize - 4];
            self.stream.read_exact(&mut startup).await.unwrap();
            self.message(b'R', &0u32.to_be_bytes()).await;
            self.message(b'K', &[0, 0, 0, 1, 0, 0, 0, 2]).await;
            self.ready(b'I').await;
        }
    }

    pub async fn query(&mut self) -> String {
        loop {
            let query = if self.mysql {
                let packet = self.read_packet().await;
                if packet == [14] {
                    // COM_PING does not modify transaction state.
                    self.complete(0, false).await;
                    continue;
                }
                assert_eq!(packet[0], 3);
                String::from_utf8(packet[1..].to_vec()).unwrap()
            } else {
                assert_eq!(self.stream.read_u8().await.unwrap(), b'Q');
                let len = self.stream.read_u32().await.unwrap();
                let mut query = vec![0; len as usize - 4];
                self.stream.read_exact(&mut query).await.unwrap();
                assert_eq!(query.pop(), Some(0));
                String::from_utf8(query).unwrap()
            };
            if query.starts_with("SET NAMES") {
                self.complete(0, false).await;
            } else if query == "SELECT @@max_allowed_packet,@@wait_timeout" {
                self.rows(
                    &[("@@max_allowed_packet", false), ("@@wait_timeout", false)],
                    &[vec![Some(b"67108864"), Some(b"28800")]],
                    false,
                )
                .await;
            } else if query.is_empty() {
                self.message(b'I', &[]).await;
                self.ready(b'I').await;
            } else {
                return query;
            }
        }
    }

    pub async fn complete(&mut self, affected: u8, more: bool) {
        if self.mysql {
            self.packet(&[0, affected, 0, if more { 10 } else { 2 }, 0, 0, 0])
                .await;
        } else {
            self.message(b'C', format!("UPDATE {affected}\0").as_bytes())
                .await;
            if !more {
                self.ready(b'I').await;
            }
        }
    }

    pub async fn ready(&mut self, status: u8) {
        self.message(b'Z', &[status]).await;
    }

    pub async fn rows(
        &mut self,
        columns: &[(&str, bool)],
        rows: &[Vec<Option<&[u8]>>],
        more: bool,
    ) {
        if self.mysql {
            self.packet(&[columns.len() as u8]).await;
            for (name, binary) in columns {
                let mut column = b"\x03def\0\0\0".to_vec();
                column.push(name.len() as u8);
                column.extend(name.as_bytes());
                column.extend([0, 12, if *binary { 63 } else { 45 }, 0]);
                column.extend(1024u32.to_le_bytes());
                column.extend([253, if *binary { 128 } else { 0 }, 0, 0, 0, 0]);
                self.packet(&column).await;
            }
            self.packet(&[254, 0, 0, 2, 0]).await;
            for row in rows {
                let mut packet = Vec::new();
                for value in row {
                    if let Some(value) = value {
                        assert!(value.len() < 251);
                        packet.push(value.len() as u8);
                        packet.extend(*value);
                    } else {
                        packet.push(251);
                    }
                }
                self.packet(&packet).await;
            }
            self.packet(&[254, 0, 0, if more { 10 } else { 2 }, 0])
                .await;
        } else {
            let mut description = (columns.len() as u16).to_be_bytes().to_vec();
            for (name, _) in columns {
                description.extend(name.as_bytes());
                description.push(0);
                description.extend(0u32.to_be_bytes());
                description.extend(0u16.to_be_bytes());
                description.extend(25u32.to_be_bytes());
                description.extend((-1i16).to_be_bytes());
                description.extend((-1i32).to_be_bytes());
                description.extend(0u16.to_be_bytes());
            }
            self.message(b'T', &description).await;
            for row in rows {
                let mut packet = (row.len() as u16).to_be_bytes().to_vec();
                for value in row {
                    packet.extend(value.map_or(-1, |value| value.len() as i32).to_be_bytes());
                    if let Some(value) = value {
                        packet.extend(*value);
                    }
                }
                self.message(b'D', &packet).await;
            }
            self.message(b'C', format!("SELECT {}\0", rows.len()).as_bytes())
                .await;
            if !more {
                self.ready(b'I').await;
            }
        }
    }
}
