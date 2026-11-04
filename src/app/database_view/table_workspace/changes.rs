use super::{CellMode, CellValue, RowChange};
use rustterm::database::DatabaseError;
use std::collections::BTreeMap;

pub(super) fn initial_mode(
    change: Option<&RowChange>,
    column: usize,
    value: Option<&str>,
) -> CellMode {
    if matches!(change, Some(RowChange::Insert { values }) if !values.iter().any(|(index, _)| *index == column))
    {
        CellMode::Default
    } else if value.is_none() {
        CellMode::Null
    } else {
        CellMode::Value
    }
}

/// None means omit an inserted field, while Some(Null) writes SQL NULL.
pub(super) fn stage_value(
    changes: &mut BTreeMap<usize, RowChange>,
    originals: &[Vec<CellValue>],
    row: usize,
    column: usize,
    value: Option<CellValue>,
) -> Result<Option<String>, DatabaseError> {
    let Some(value) = value else {
        let Some(RowChange::Insert { values }) = changes.get_mut(&row) else {
            return Err(DatabaseError::InvalidTableEdit);
        };
        values.retain(|(index, _)| *index != column);
        return Ok(None);
    };
    if let Some(RowChange::Insert { values }) = changes.get_mut(&row) {
        values.retain(|(index, _)| *index != column);
        values.push((column, value.clone()));
        return Ok(value.display());
    }
    if matches!(changes.get(&row), Some(RowChange::Delete { .. })) {
        return Err(DatabaseError::InvalidTableEdit);
    }
    let original = originals.get(row).ok_or(DatabaseError::InvalidTableEdit)?;
    let previous = original
        .get(column)
        .ok_or(DatabaseError::InvalidTableEdit)?;
    let change = changes.entry(row).or_insert_with(|| RowChange::Update {
        original: original.clone(),
        values: Vec::new(),
    });
    let RowChange::Update { values, .. } = change else {
        return Err(DatabaseError::InvalidTableEdit);
    };
    values.retain(|(index, _)| *index != column);
    if previous != &value {
        values.push((column, value.clone()));
    }
    if values.is_empty() {
        changes.remove(&row);
    }
    Ok(value.display())
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum DeleteEffect {
    RemoveInserted,
    ShowOriginal,
}

pub(super) fn toggle_delete(
    changes: &mut BTreeMap<usize, RowChange>,
    originals: &[Vec<CellValue>],
    row: usize,
) -> Result<DeleteEffect, DatabaseError> {
    if matches!(changes.get(&row), Some(RowChange::Insert { .. })) {
        changes.remove(&row);
        *changes = std::mem::take(changes)
            .into_iter()
            .map(|(index, change)| (if index > row { index - 1 } else { index }, change))
            .collect();
        return Ok(DeleteEffect::RemoveInserted);
    }
    let original = originals.get(row).ok_or(DatabaseError::InvalidTableEdit)?;
    if matches!(changes.get(&row), Some(RowChange::Delete { .. })) {
        changes.remove(&row);
    } else {
        changes.insert(
            row,
            RowChange::Delete {
                original: original.clone(),
            },
        );
    }
    Ok(DeleteEffect::ShowOriginal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverting_a_cell_removes_only_its_pending_assignment() {
        let originals = vec![vec![
            CellValue::Integer("1".into()),
            CellValue::Text("before".into()),
            CellValue::Null,
        ]];
        let mut changes = BTreeMap::new();
        stage_value(
            &mut changes,
            &originals,
            0,
            1,
            Some(CellValue::Text("after".into())),
        )
        .unwrap();
        stage_value(
            &mut changes,
            &originals,
            0,
            2,
            Some(CellValue::Text(String::new())),
        )
        .unwrap();
        stage_value(
            &mut changes,
            &originals,
            0,
            1,
            Some(originals[0][1].clone()),
        )
        .unwrap();
        assert_eq!(
            changes.get(&0),
            Some(&RowChange::Update {
                original: originals[0].clone(),
                values: vec![(2, CellValue::Text(String::new()))],
            })
        );
        stage_value(&mut changes, &originals, 0, 2, Some(CellValue::Null)).unwrap();
        assert!(changes.is_empty());
    }

    #[test]
    fn inserted_default_null_and_empty_string_are_distinct() {
        let mut changes = BTreeMap::from([(0, RowChange::Insert { values: Vec::new() })]);
        assert!(initial_mode(changes.get(&0), 1, None) == CellMode::Default);
        stage_value(&mut changes, &[], 0, 1, Some(CellValue::Null)).unwrap();
        assert!(initial_mode(changes.get(&0), 1, None) == CellMode::Null);
        assert_eq!(
            changes.get(&0),
            Some(&RowChange::Insert {
                values: vec![(1, CellValue::Null)]
            })
        );
        assert_eq!(
            stage_value(
                &mut changes,
                &[],
                0,
                1,
                Some(CellValue::Text(String::new()))
            )
            .unwrap(),
            Some(String::new())
        );
        assert!(initial_mode(changes.get(&0), 1, Some("")) == CellMode::Value);
        stage_value(&mut changes, &[], 0, 1, None).unwrap();
        assert_eq!(
            changes.get(&0),
            Some(&RowChange::Insert { values: Vec::new() })
        );
        assert!(initial_mode(changes.get(&0), 1, None) == CellMode::Default);
    }

    #[test]
    fn deleting_and_restoring_an_existing_row_preserves_its_original_identity() {
        let originals = vec![vec![
            CellValue::Integer("1".into()),
            CellValue::Text("before".into()),
        ]];
        let mut changes = BTreeMap::new();
        stage_value(
            &mut changes,
            &originals,
            0,
            0,
            Some(CellValue::Integer("2".into())),
        )
        .unwrap();
        assert_eq!(
            toggle_delete(&mut changes, &originals, 0).unwrap(),
            DeleteEffect::ShowOriginal
        );
        assert_eq!(
            changes.get(&0),
            Some(&RowChange::Delete {
                original: originals[0].clone()
            })
        );
        assert!(stage_value(&mut changes, &originals, 0, 1, Some(CellValue::Null)).is_err());
        assert_eq!(
            toggle_delete(&mut changes, &originals, 0).unwrap(),
            DeleteEffect::ShowOriginal
        );
        assert!(changes.is_empty());
        assert_eq!(originals[0][0], CellValue::Integer("1".into()));
    }

    #[test]
    fn removing_an_inserted_row_reindexes_later_drafts_without_changing_originals() {
        let originals = vec![vec![CellValue::Integer("1".into())]];
        let pending = RowChange::Insert {
            values: vec![(0, CellValue::Integer("3".into()))],
        };
        let mut changes = BTreeMap::from([
            (1, RowChange::Insert { values: Vec::new() }),
            (2, pending.clone()),
        ]);
        assert_eq!(
            toggle_delete(&mut changes, &originals, 1).unwrap(),
            DeleteEffect::RemoveInserted
        );
        assert_eq!(changes, BTreeMap::from([(1, pending)]));
        assert!(stage_value(&mut changes, &originals, 0, 0, None).is_err());
    }

    #[test]
    fn cell_editor_rejects_values_it_cannot_preserve_exactly() {
        use crate::app::text_input::can_edit_literal;
        assert!(can_edit_literal("\t中\n  text "));
        assert!(can_edit_literal(&"x".repeat(65_536)));
        assert!(!can_edit_literal(&"x".repeat(65_537)));
        for value in ["line\r\n", "nul\0byte", "escape\x1b[31m", "next\u{85}line"] {
            assert!(!can_edit_literal(value));
        }
    }
}
