use super::wire::{MTU, Parity, WINDOW};
use crate::{Error, Result};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::OnceLock,
};

const MAX_EQUATIONS: usize = 16;

pub(super) struct Recovery {
    sources: BTreeMap<u64, Vec<u8>>,
    equations: VecDeque<Parity>,
}

impl Recovery {
    pub fn new() -> Self {
        Self {
            sources: BTreeMap::new(),
            equations: VecDeque::new(),
        }
    }
    pub fn contains(&self, sequence: u64) -> bool {
        self.sources.contains_key(&sequence)
    }
    pub fn insert(&mut self, sequence: u64, bytes: Vec<u8>) {
        self.sources.insert(sequence, bytes);
        while self.sources.len() > WINDOW {
            self.sources.pop_first();
        }
    }
    pub fn parity(&mut self, parity: Parity, expected: u64) -> Result<Vec<(u64, Vec<u8>)>> {
        if parity.start + parity.count as u64 <= expected.saturating_sub(WINDOW as u64)
            || parity.start >= expected.saturating_add(WINDOW as u64)
        {
            return Ok(Vec::new());
        }
        if self.equations.len() == MAX_EQUATIONS {
            self.equations.pop_front();
        }
        self.equations.push_back(parity);
        self.recover(expected)
    }
    pub fn recover(&mut self, expected: u64) -> Result<Vec<(u64, Vec<u8>)>> {
        self.equations.retain(|equation| {
            equation.start + equation.count as u64 > expected.saturating_sub(WINDOW as u64)
                && (0..equation.count)
                    .any(|offset| !self.sources.contains_key(&(equation.start + offset as u64)))
        });
        let mut unknown = BTreeSet::new();
        let mut usable = Vec::new();
        for equation in &self.equations {
            let missing: Vec<_> = (0..equation.count)
                .map(|offset| equation.start + offset as u64)
                .filter(|sequence| !self.sources.contains_key(sequence))
                .collect();
            if missing.len() > MAX_EQUATIONS {
                continue;
            }
            let additional = missing
                .iter()
                .filter(|sequence| !unknown.contains(*sequence))
                .count();
            if unknown.len() + additional > MAX_EQUATIONS {
                continue;
            }
            unknown.extend(missing);
            usable.push(equation);
        }
        if unknown.is_empty() {
            return Ok(Vec::new());
        }
        let unknown: Vec<_> = unknown.into_iter().collect();
        let size = usable.iter().map(|row| row.bytes.len()).max().unwrap_or(0);
        if !(2..=MTU + 2).contains(&size) {
            return Err(Error::Invalid("UDP FEC equation length"));
        }
        let mut rows = Vec::<(Vec<u8>, Vec<u8>)>::new();
        for equation in usable {
            let mut coefficients = vec![0; unknown.len()];
            let mut bytes = equation.bytes.clone();
            bytes.resize(size, 0);
            for offset in 0..equation.count {
                let sequence = equation.start + offset as u64;
                let coefficient = inverse(equation.index ^ sequence as u8);
                if let Some(source) = self.sources.get(&sequence) {
                    if source.len() + 2 > bytes.len() {
                        return Err(Error::Invalid("UDP FEC source length"));
                    }
                    for (position, byte) in (source.len() as u16)
                        .to_be_bytes()
                        .into_iter()
                        .chain(source.iter().copied())
                        .enumerate()
                    {
                        bytes[position] ^= multiply(coefficient, byte);
                    }
                } else if let Ok(column) = unknown.binary_search(&sequence) {
                    coefficients[column] = coefficient;
                }
            }
            rows.push((coefficients, bytes));
        }
        let mut rank = 0;
        for column in 0..unknown.len() {
            let Some(pivot) = (rank..rows.len()).find(|row| rows[*row].0[column] != 0) else {
                continue;
            };
            rows.swap(rank, pivot);
            let scale = inverse(rows[rank].0[column]);
            for value in &mut rows[rank].0 {
                *value = multiply(*value, scale);
            }
            for value in &mut rows[rank].1 {
                *value = multiply(*value, scale);
            }
            let (before, pivot_and_after) = rows.split_at_mut(rank);
            let (pivot, after) = pivot_and_after
                .split_first_mut()
                .ok_or(Error::Invalid("UDP FEC pivot"))?;
            for row in before.iter_mut().chain(after.iter_mut()) {
                let factor = row.0[column];
                if factor == 0 {
                    continue;
                }
                for (value, known) in row.0.iter_mut().zip(&pivot.0) {
                    *value ^= multiply(*known, factor);
                }
                for (value, known) in row.1.iter_mut().zip(&pivot.1) {
                    *value ^= multiply(*known, factor);
                }
            }
            rank += 1;
        }
        let mut recovered = Vec::new();
        for (coefficients, bytes) in rows {
            let mut nonzero = coefficients
                .iter()
                .enumerate()
                .filter(|(_, value)| **value != 0);
            let Some((column, coefficient)) = nonzero.next() else {
                continue;
            };
            if nonzero.next().is_some() || *coefficient != 1 {
                continue;
            }
            let length = usize::from(u16::from_be_bytes([bytes[0], bytes[1]]));
            if length > bytes.len() - 2 || length > MTU {
                return Err(Error::Invalid("recovered UDP FEC length"));
            }
            recovered.push((unknown[column], bytes[2..2 + length].to_vec()));
        }
        Ok(recovered)
    }
}

pub(super) fn encode(sources: &[(u64, &[u8])], index: u8) -> Vec<u8> {
    let size = sources
        .iter()
        .map(|(_, bytes)| bytes.len() + 2)
        .max()
        .unwrap_or(0);
    let mut encoded = vec![0; size];
    for (sequence, bytes) in sources {
        let coefficient = inverse(index ^ *sequence as u8);
        for (position, byte) in (bytes.len() as u16)
            .to_be_bytes()
            .into_iter()
            .chain(bytes.iter().copied())
            .enumerate()
        {
            encoded[position] ^= multiply(coefficient, byte);
        }
    }
    encoded
}

fn tables() -> &'static ([u8; 512], [u8; 256]) {
    static TABLES: OnceLock<([u8; 512], [u8; 256])> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut exponent = [0; 512];
        let mut logarithm = [0; 256];
        let mut value = 1u8;
        for (index, entry) in exponent[..255].iter_mut().enumerate() {
            *entry = value;
            logarithm[usize::from(value)] = index as u8;
            value = (value << 1) ^ if value & 0x80 != 0 { 0x1d } else { 0 };
        }
        for index in 255..512 {
            exponent[index] = exponent[index - 255];
        }
        (exponent, logarithm)
    })
}
fn multiply(left: u8, right: u8) -> u8 {
    if left == 0 || right == 0 {
        return 0;
    }
    let (exponent, logarithm) = tables();
    exponent[usize::from(logarithm[usize::from(left)]) + usize::from(logarithm[usize::from(right)])]
}
fn inverse(value: u8) -> u8 {
    if value == 0 {
        return 0;
    }
    let (exponent, logarithm) = tables();
    exponent[255 - usize::from(logarithm[usize::from(value)])]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn solves_two_lost_sources_and_variable_lengths() {
        let source = [
            (10u64, b"first".as_slice()),
            (11, b"second longer".as_slice()),
            (12, b"third".as_slice()),
        ];
        let mut recovery = Recovery::new();
        recovery.insert(11, source[1].1.to_vec());
        assert!(
            recovery
                .parity(
                    Parity {
                        start: 10,
                        count: 3,
                        index: 14,
                        bytes: encode(&source, 14)
                    },
                    10
                )
                .unwrap()
                .is_empty()
        );
        let result = recovery
            .parity(
                Parity {
                    start: 10,
                    count: 3,
                    index: 15,
                    bytes: encode(&source, 15),
                },
                10,
            )
            .unwrap();
        assert_eq!(result, [(10, b"first".to_vec()), (12, b"third".to_vec())]);
    }
    #[test]
    fn finite_field_inverse_and_malformed_lengths() {
        for value in 1..=255 {
            assert_eq!(multiply(value, inverse(value)), 1);
        }
        let mut recovery = Recovery::new();
        assert!(
            recovery
                .parity(
                    Parity {
                        start: 1,
                        count: 1,
                        index: 2,
                        bytes: vec![255, 255]
                    },
                    1
                )
                .is_err()
        );
    }
}
