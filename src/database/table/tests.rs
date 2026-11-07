use super::*;
use crate::{
    config::{DatabaseConfig, Secret, SqliteConfig},
    database::create_sqlite,
};

fn table(name: &str) -> TableInfo {
    TableInfo {
        schema: "main".into(),
        name: name.into(),
        kind: "TABLE".into(),
    }
}

async fn sqlite() -> (tempfile::TempDir, DatabaseClient) {
    let directory = tempfile::tempdir().unwrap();
    let config = SqliteConfig {
        path: directory.path().join("table.sqlite"),
        read_only: false,
        query_timeout_secs: 10,
    };
    create_sqlite(config.clone()).await.unwrap();
    let client = DatabaseClient::connect(
        DatabaseConfig::Sqlite(config),
        Secret::new(String::new()),
        None,
    )
    .await
    .unwrap();
    (directory, client)
}

#[test]
fn numeric_input_is_exact_and_cannot_become_sql() {
    for text in ["123456789012345678901234567890", "-0", "+23"] {
        assert!(statements::valid_number(text, true));
    }
    for text in [
        "1.25",
        ".5",
        "-2E+99",
        "999999999999999999999.1234567890123456789",
    ] {
        assert!(statements::valid_number(text, false));
        assert!(!statements::valid_number(text, true));
    }
    for text in [
        "",
        "1; DELETE FROM t",
        "1 OR 1=1",
        "--1",
        "1e",
        "NaN",
        "Infinity",
        " 1",
        "1 ",
    ] {
        assert!(!statements::valid_number(text, false));
    }
    assert_eq!(statements::decode_hex("\\x00ff").unwrap(), [0, 255]);
    assert!(statements::decode_hex("0x0g").is_err());
}

#[tokio::test]
async fn pages_filter_server_side_and_keep_composite_key_order() {
    let (_directory, client) = sqlite().await;
    client.execute("CREATE TABLE items(tenant INTEGER NOT NULL,id INTEGER NOT NULL,name TEXT,PRIMARY KEY(tenant,id)); INSERT INTO items VALUES (2,1,'late'),(1,3,'100%'),(1,1,'100x'),(1,2,'100%');".into(), 20).await.unwrap();
    let schema = client.table_schema(table("items")).await.unwrap();
    assert_eq!(schema.primary_key(), [0, 1]);
    assert!(schema.editable());
    let largest_page = client
        .table_page(
            &schema,
            PageRequest {
                limit: MAX_ROWS,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(largest_page.result.rows.len(), 4);
    assert!(!largest_page.has_next);
    let first = client
        .table_page(
            &schema,
            PageRequest {
                limit: 2,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(first.has_next);
    assert_eq!(first.result.rows[0][1].as_deref(), Some("1"));
    assert_eq!(first.result.rows[1][1].as_deref(), Some("2"));
    let second = client
        .table_page(
            &schema,
            PageRequest {
                offset: 2,
                limit: 2,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(!second.has_next);
    assert_eq!(second.result.rows[0][1].as_deref(), Some("3"));
    assert_eq!(second.result.rows[1][0].as_deref(), Some("2"));
    let filtered = client
        .table_page(
            &schema,
            PageRequest {
                filter: Some(ColumnFilter {
                    column: 2,
                    operator: FilterOperator::Contains,
                    value: CellValue::Text("100%".into()),
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(filtered.result.rows.len(), 2);
    assert!(
        filtered
            .result
            .rows
            .iter()
            .all(|row| row[2].as_deref() == Some("100%"))
    );
    assert!(client.is_connected());
}

#[tokio::test]
async fn row_changes_preserve_null_text_blobs_and_rollback_on_conflict() {
    let (_directory, client) = sqlite().await;
    client.execute("CREATE TABLE items(id INTEGER PRIMARY KEY,label TEXT COLLATE NOCASE,value BLOB,optional TEXT); INSERT INTO items VALUES(1,'one',X'00ff',NULL),(2,'two',X'0102','');".into(), 20).await.unwrap();
    let schema = client.table_schema(table("items")).await.unwrap();
    let page = client
        .table_page(&schema, PageRequest::default())
        .await
        .unwrap();
    assert_eq!(page.originals[0][2], CellValue::Binary(vec![0, 255]));
    assert_eq!(page.originals[0][3], CellValue::Null);
    assert_eq!(page.originals[1][3], CellValue::Text(String::new()));
    client
        .execute("UPDATE items SET label='TWO' WHERE id=2".into(), 10)
        .await
        .unwrap();
    let changes = vec![
        RowChange::Update {
            original: page.originals[0].clone(),
            values: vec![(1, CellValue::Text("must roll back".into()))],
        },
        RowChange::Update {
            original: page.originals[1].clone(),
            values: vec![(1, CellValue::Text("stale".into()))],
        },
    ];
    assert_eq!(
        client.apply_changes(&schema, changes).await.unwrap_err(),
        DatabaseError::Conflict
    );
    let page = client
        .table_page(&schema, PageRequest::default())
        .await
        .unwrap();
    assert_eq!(page.result.rows[0][1].as_deref(), Some("one"));
    assert_eq!(page.result.rows[1][1].as_deref(), Some("TWO"));
    let literal = "quoted' ; DELETE FROM items; --\0text";
    client
        .apply_changes(
            &schema,
            vec![
                RowChange::Update {
                    original: page.originals[0].clone(),
                    values: vec![
                        (1, CellValue::Text(literal.into())),
                        (2, CellValue::Binary(vec![0, 1, 254])),
                    ],
                },
                RowChange::Delete {
                    original: page.originals[1].clone(),
                },
                RowChange::Insert {
                    values: vec![(1, CellValue::Text("new".into()))],
                },
            ],
        )
        .await
        .unwrap();
    let page = client
        .table_page(&schema, PageRequest::default())
        .await
        .unwrap();
    assert_eq!(page.result.rows.len(), 2);
    assert_eq!(page.result.rows[0][1].as_deref(), Some(literal));
    assert_eq!(page.originals[0][2], CellValue::Binary(vec![0, 1, 254]));
    assert!(client.is_connected());
}

#[tokio::test]
async fn sqlite_invalid_text_remains_viewable_without_becoming_an_editable_blob() {
    let (_directory, client) = sqlite().await;
    client.execute("CREATE TABLE items(id INTEGER PRIMARY KEY,label TEXT); INSERT INTO items VALUES(1,CAST(X'80ff' AS TEXT)),(2,'0x80ff');".into(), 10).await.unwrap();
    let schema = client.table_schema(table("items")).await.unwrap();
    let page = client
        .table_page(&schema, PageRequest::default())
        .await
        .unwrap();
    assert_eq!(page.result.rows[0][1].as_deref(), Some("0x80ff"));
    assert_eq!(page.result.rows[1][1].as_deref(), Some("0x80ff"));
    assert_eq!(page.originals[0][1], CellValue::InvalidText(vec![128, 255]));
    assert!(!schema.row_editable(&page.originals[0]));
    assert!(schema.row_editable(&page.originals[1]));
}

#[tokio::test]
async fn sqlite_cell_edits_preserve_dynamic_storage_classes() {
    let (_directory, client) = sqlite().await;
    client.execute("CREATE TABLE items(id INTEGER PRIMARY KEY,value); INSERT INTO items VALUES(1,123),(2,'123'),(3,1.25),(4,X'0102');".into(), 10).await.unwrap();
    let schema = client.table_schema(table("items")).await.unwrap();
    let page = client
        .table_page(&schema, PageRequest::default())
        .await
        .unwrap();
    let inputs = ["456", "456", "2.5", "0x0304"];
    let values = page
        .originals
        .iter()
        .zip(inputs)
        .map(|(row, text)| schema.parse_cell(1, Some(&row[1]), text).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        values,
        [
            CellValue::Integer("456".into()),
            CellValue::Text("456".into()),
            CellValue::Real("2.5".into()),
            CellValue::Binary(vec![3, 4])
        ]
    );
    client
        .apply_changes(
            &schema,
            page.originals
                .into_iter()
                .zip(values)
                .map(|(original, value)| RowChange::Update {
                    original,
                    values: vec![(1, value)],
                })
                .collect(),
        )
        .await
        .unwrap();
    let output = client
        .execute("SELECT typeof(value) FROM items ORDER BY id".into(), 10)
        .await
        .unwrap();
    assert_eq!(
        output.results[0]
            .rows
            .iter()
            .map(|row| row[0].as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["integer", "text", "real", "blob"]
    );
}

#[tokio::test]
async fn views_and_tables_without_keys_cannot_generate_mutations() {
    let (_directory, client) = sqlite().await;
    client
        .execute(
            "CREATE TABLE items(value TEXT); CREATE VIEW item_view AS SELECT value FROM items;"
                .into(),
            10,
        )
        .await
        .unwrap();
    for name in ["items", "item_view"] {
        let schema = client.table_schema(table(name)).await.unwrap();
        assert!(!schema.editable());
        assert!(schema.read_only_reason.is_some());
        assert!(matches!(
            client.preview_changes(
                &schema,
                &[RowChange::Insert {
                    values: vec![(0, CellValue::Text("x".into()))]
                }]
            ),
            Err(DatabaseError::TableReadOnly)
        ));
    }
}

#[tokio::test]
async fn sqlite_object_details_definition_and_foreign_keys_use_quoted_names() {
    let (_directory, client) = sqlite().await;
    client.execute("CREATE TABLE parent(id INTEGER PRIMARY KEY); CREATE TABLE \"child'quoted\"(id INTEGER PRIMARY KEY,parent_id INTEGER REFERENCES parent(id)); CREATE VIEW summary AS SELECT * FROM \"child'quoted\";".into(), 10).await.unwrap();
    let table = table("child'quoted");
    let details = client.table_details(table.clone()).await.unwrap();
    assert_eq!(details.results[0].rows[0][0].as_deref(), Some("table"));
    let definition = client.definition(table.clone()).await.unwrap();
    assert!(
        definition.results[0].rows[0][0]
            .as_ref()
            .unwrap()
            .contains("REFERENCES parent(id)")
    );
    let keys = client.constraints(table.clone()).await.unwrap();
    assert_eq!(keys.results[0].rows.len(), 1);
    assert!(keys.results[0].rows[0].contains(&Some("parent".into())));
    let mut view = table.clone();
    view.name = "summary".into();
    view.kind = "VIEW".into();
    assert!(
        client.definition(view).await.unwrap().results[0].rows[0][0]
            .as_ref()
            .unwrap()
            .starts_with("CREATE VIEW")
    );
    let mut invalid = table;
    invalid.schema = "main; DROP TABLE parent".into();
    assert_eq!(
        client.table_details(invalid.clone()).await.unwrap_err(),
        DatabaseError::InvalidConfig
    );
    assert_eq!(
        client.definition(invalid).await.unwrap_err(),
        DatabaseError::InvalidConfig
    );
}
