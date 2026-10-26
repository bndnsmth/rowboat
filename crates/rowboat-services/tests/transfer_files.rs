mod common;
use anyhow::{Result, ensure};
use common::peak_kib;
use rowboat_core::Cell;
use rowboat_services::*;
use std::io::Write;

#[tokio::test]
async fn preview_streams_exact_csv_jsonlines_and_bounded_samples() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let csv = directory.path().join("data.csv");
    std::fs::write(
        &csv,
        "id,value,bytes\r\n1,\\N,\\x00ff\r\n2,NULL,\\x\r\n3,\"comma,quote\"\"\n日本語\",\\\\N\r\n",
    )?;
    let options = TransferOptions {
        sample_rows: 2,
        ..Default::default()
    };
    let preview = preview_import(
        &csv,
        TransferFormat::Csv,
        &options,
        &TransferControl::default(),
    )
    .await?;
    ensure!(
        preview.summary.rows == 3 && preview.sample.len() == 2,
        "Sample was not bounded"
    );
    assert_eq!(preview.sample[0][1].1, Cell::Null);
    assert_eq!(preview.sample[1][1].1, Cell::Text("NULL".into()));
    assert_eq!(preview.sample[0][2].1, Cell::Binary(vec![0, 255]));
    let jsonl = directory.path().join("data.jsonl");
    std::fs::write(
        &jsonl,
        "{\"a\":123456789012345678901234567890.1234567890123456789,\"b\":null}\n{\"b\":\"NULL\",\"a\":{\"$binary\":\"00ff\"}}\n",
    )?;
    let preview = preview_import(
        &jsonl,
        TransferFormat::JsonLines,
        &options,
        &TransferControl::default(),
    )
    .await?;
    assert_eq!(
        preview.sample[0][0].1,
        Cell::Text("123456789012345678901234567890.1234567890123456789".into())
    );
    assert_eq!(preview.sample[1][0].1, Cell::Binary(vec![0, 255]));
    assert_eq!(preview.summary.bytes, std::fs::metadata(jsonl)?.len());
    Ok(())
}

#[tokio::test]
async fn strict_incremental_parsers_reject_late_errors_and_oversized_records() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("data");
    for (format, input) in [
        (TransferFormat::Csv, "a,b\n1,2\n3\n"),
        (TransferFormat::Csv, "a,a\n1,2\n"),
        (TransferFormat::Csv, "a\n\"unterminated"),
        (TransferFormat::Csv, "a\nabc\"def\n"),
        (TransferFormat::Csv, "a\n\"valid\"invalid\n"),
        (TransferFormat::Csv, "a\n\\xGG\n"),
        (TransferFormat::JsonLines, "{\"a\":1}\n{\"b\":2}\n"),
        (TransferFormat::JsonLines, "{\"a\":1}\n{\"a\":1,\"a\":2}\n"),
        (TransferFormat::JsonLines, "[{\"a\":1}]\n"),
        (TransferFormat::Sql, "SELECT 1;"),
    ] {
        std::fs::write(&path, input)?;
        assert!(
            preview_import(
                &path,
                format,
                &TransferOptions::default(),
                &TransferControl::default()
            )
            .await
            .is_err()
        );
    }
    std::fs::write(&path, format!("a\n{}\n", "x".repeat(300)))?;
    let options = TransferOptions {
        max_record_bytes: 256,
        ..Default::default()
    };
    assert!(
        preview_import(
            &path,
            TransferFormat::Csv,
            &options,
            &TransferControl::default()
        )
        .await
        .is_err()
    );
    let control = TransferControl::default();
    control.cancel();
    assert!(
        preview_import(
            &path,
            TransferFormat::Csv,
            &TransferOptions::default(),
            &control
        )
        .await
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn source_requires_a_regular_file_without_following_symlinks() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("source.csv");
    std::fs::write(&path, "id\n1\n")?;
    let link = directory.path().join("link.csv");
    std::os::unix::fs::symlink(&path, &link)?;
    for source in [link.as_path(), directory.path()] {
        ensure!(
            preview_import(
                source,
                TransferFormat::Csv,
                &TransferOptions::default(),
                &TransferControl::default()
            )
            .await
            .is_err(),
            "Accepted a nonregular/symlink source"
        );
    }
    Ok(())
}

#[tokio::test]
async fn csv_bom_is_only_an_optional_file_prefix_not_a_data_prefix() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("bom.csv");
    let expected = vec![
        vec![("name".into(), Cell::Text("\u{feff}Alice".into()))],
        vec![("name".into(), Cell::Text("\u{feff}".into()))],
    ];
    for header in ["name", "\u{feff}name", "\u{feff}\"name\""] {
        std::fs::write(&path, format!("{header}\n\u{feff}Alice\n\u{feff}\n"))?;
        let preview = preview_import(
            &path,
            TransferFormat::Csv,
            &TransferOptions::default(),
            &TransferControl::default(),
        )
        .await?;
        assert_eq!(preview.summary.rows, 2);
        assert_eq!(preview.sample, expected);
        let result = rowboat_core::QueryResult {
            columns: vec![rowboat_core::Column {
                name: "name".into(),
                data_type: "TEXT".into(),
                nullable: true,
                primary_key: false,
                default_value: None,
            }],
            rows: preview
                .sample
                .into_iter()
                .map(|r| r.into_iter().map(|(_, cell)| cell).collect())
                .collect(),
            ..Default::default()
        };
        std::fs::write(&path, export_csv(&result)?)?;
        let again = preview_import(
            &path,
            TransferFormat::Csv,
            &TransferOptions::default(),
            &TransferControl::default(),
        )
        .await?;
        assert_eq!(again.sample, expected);
    }
    Ok(())
}

#[test]
fn generated_large_preview_has_measured_bounded_peak_memory() -> Result<()> {
    if std::env::var_os("ROWBOAT_TRANSFER_MEMORY_CHILD").is_none() {
        let status = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "generated_large_preview_has_measured_bounded_peak_memory",
                "--nocapture",
            ])
            .env("ROWBOAT_TRANSFER_MEMORY_CHILD", "1")
            .status()?;
        ensure!(status.success(), "Bounded-memory preview subprocess failed");
        return Ok(());
    }
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("large.csv");
    let mut file = std::io::BufWriter::new(std::fs::File::create(&path)?);
    file.write_all(b"id,payload\n")?;
    let payload = "x".repeat(1024);
    for id in 0..65_536 {
        writeln!(file, "{id},{payload}")?;
    }
    file.flush()?;
    drop(file);
    let before = peak_kib()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let preview = runtime.block_on(preview_import(
        &path,
        TransferFormat::Csv,
        &TransferOptions::default(),
        &TransferControl::default(),
    ))?;
    let growth = peak_kib()?.saturating_sub(before);
    ensure!(
        preview.summary.rows == 65_536 && preview.summary.bytes > 64 * 1024 * 1024,
        "Large fixture did not scan fully"
    );
    ensure!(
        preview.sample.len() == 50,
        "Large preview retained excess rows"
    );
    ensure!(
        growth < 32 * 1024,
        "Peak RSS grew by {growth} KiB while scanning 64MiB; whole-file buffering regression"
    );
    Ok(())
}
