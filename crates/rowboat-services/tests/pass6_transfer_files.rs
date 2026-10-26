mod common;
use anyhow::{Result, ensure};
use common::peak_kib;
use rowboat_core::Cell;
use rowboat_services::{TransferControl, TransferFormat, TransferOptions, preview_import};
use std::io::Write;

async fn preview(
    input: &[u8],
    options: &TransferOptions,
) -> Result<rowboat_services::ImportPreview> {
    let mut file = tempfile::NamedTempFile::new()?;
    file.write_all(input)?;
    preview_import(
        file.path(),
        TransferFormat::JsonArray,
        options,
        &TransferControl::default(),
    )
    .await
}

#[tokio::test]
async fn arrays_preserve_exact_numbers_null_empty_binary_and_escaped_strings() -> Result<()> {
    let input = br#"[
      {"z":null,"n":123456789012345678901234567890.1234567890123456789,"b":{"$binary":"00Ff"},"s":"","t":true},
      {"s":"\ufeff\ud83d\ude80, [} \" \\ \n","b":{"$binary":""},"n":-1.234567890123456789e+1234,"z":"NULL","t":false}
    ] "#;
    let mut input = input.to_vec();
    input.extend_from_slice(b"\r\n");
    let result = preview(&input, &TransferOptions::default()).await?;
    assert_eq!(result.columns, ["b", "n", "s", "t", "z"]);
    assert_eq!(result.summary.rows, 2);
    assert_eq!(result.summary.bytes, input.len() as u64);
    assert_eq!(
        result.sample[0],
        vec![
            ("b".into(), Cell::Binary(vec![0, 255])),
            (
                "n".into(),
                Cell::Text("123456789012345678901234567890.1234567890123456789".into())
            ),
            ("s".into(), Cell::Text("".into())),
            ("t".into(), Cell::Text("true".into())),
            ("z".into(), Cell::Null),
        ]
    );
    assert_eq!(result.sample[1][0].1, Cell::Binary(vec![]));
    assert_eq!(
        result.sample[1][1].1,
        Cell::Text("-1.234567890123456789e+1234".into())
    );
    assert_eq!(
        result.sample[1][2].1,
        Cell::Text("\u{feff}🚀, [} \" \\ \n".into())
    );
    assert_eq!(result.sample[1][4].1, Cell::Text("NULL".into()));
    Ok(())
}

#[tokio::test]
async fn arrays_reject_invalid_tails_duplicates_and_unsupported_shapes() {
    for input in [
        "",
        "[]",
        "[{}]",
        "{}",
        "null",
        "[1]",
        "[[1]]",
        "[{\"a\":1}",
        "[{\"a\":1},]",
        "[{\"a\":1} {\"a\":2}]",
        "[{\"a\":1},,{\"a\":2}]",
        "[{\"a\":1}] garbage",
        "[{\"a\":1}][{\"a\":2}]",
        "[{\"a\":1}]\0",
        "[{\"a\":1},{\"b\":2}]",
        "[{\"a\":1,\"\\u0061\":2}]",
        "[{\"a\":{\"$binary\":\"00\",\"$binary\":\"ff\"}}]",
        "[{\"a\":{\"$binary\":\"00\",\"other\":1}}]",
        "[{\"a\":{\"$binary\":null}}]",
        "[{\"a\":{\"$binary\":\"xyz\"}}]",
        "[{\"a\":{\"$binary\":{\"$binary\":\"\"}}}]",
        "[{\"a\":[]}]",
        "[{\"a\":{\"$serde_json::private::Number\":\"1\"}}]",
        "[{\"a\":1,}]",
        "[{\"a\":01}]",
        "[{\"a\":NaN}]",
        "[{\"a\":1e}]",
        "[{\"a\":\"\\ud800\"}]",
        "[{\"a\":\"unterminated}]",
        "[{\"a\":\"raw\nnewline\"}]",
        "\u{feff}[{\"a\":1}]",
        " [{\"a\":1}]\u{feff}",
        "[{\"a\":1},\u{feff}{\"a\":2}]",
        "[{\"a\":1}]\u{a0}",
        "[{\"\":1}]",
        "[{\"\\u0000\":1}]",
    ] {
        assert!(
            preview(input.as_bytes(), &TransferOptions::default())
                .await
                .is_err(),
            "Accepted {input:?}"
        );
    }
    assert!(
        preview(b"[{\"a\":\"\xff\"}]", &TransferOptions::default())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn jsonlines_shares_strict_binary_and_bom_policy() -> Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    for input in [
        "\u{feff}{\"a\":1}\n",
        "{\"a\":{\"$binary\":\"00\",\"$binary\":\"ff\"}}\n",
        "{\"a\":1}\n\u{feff}{\"a\":2}\n",
        "{\"a\":1}\n\u{a0}\n",
    ] {
        file.as_file_mut().set_len(0)?;
        std::fs::write(file.path(), input)?;
        assert!(
            preview_import(
                file.path(),
                TransferFormat::JsonLines,
                &TransferOptions::default(),
                &TransferControl::default()
            )
            .await
            .is_err()
        );
    }
    std::fs::write(file.path(), "{\"a\":\"\u{feff}\"}\n")?;
    let result = preview_import(
        file.path(),
        TransferFormat::JsonLines,
        &TransferOptions::default(),
        &TransferControl::default(),
    )
    .await?;
    assert_eq!(result.sample[0][0].1, Cell::Text("\u{feff}".into()));
    Ok(())
}

#[tokio::test]
async fn utf8_escapes_binary_and_delimiters_cross_read_buffer_boundaries() -> Result<()> {
    let row = "{\"a\":\"日本🚀\\\"\\\\\\u1234\",\"b\":{\"$binary\":\"00ff\"}}";
    for split in 0..=row.len() {
        let prefix = " ".repeat(65536 - 1 - split);
        let input = format!("[{prefix}{row}, {row}]\n");
        let result = preview(input.as_bytes(), &TransferOptions::default()).await?;
        assert_eq!(result.summary.rows, 2, "split {split}");
        assert_eq!(result.sample[0], result.sample[1]);
        assert_eq!(
            result.sample[0][0].1,
            Cell::Text("日本🚀\"\\\u{1234}".into())
        );
    }
    Ok(())
}

#[tokio::test]
async fn record_column_batch_and_sample_budgets_are_independent() -> Result<()> {
    let options = TransferOptions {
        max_record_bytes: 256,
        batch_bytes: 1024,
        max_columns: 2,
        sample_rows: 1,
        max_sample_bytes: 1024,
        ..Default::default()
    };
    let exact = format!("{{\"a\":\"{}\"}}", "x".repeat(248));
    assert_eq!(exact.len(), 256);
    assert_eq!(
        preview(format!("[{exact}, {exact}]").as_bytes(), &options)
            .await?
            .sample
            .len(),
        1
    );
    assert!(
        preview(
            format!("[{exact} ]").as_bytes(),
            &TransferOptions {
                max_sample_bytes: 0,
                ..options.clone()
            }
        )
        .await?
        .sample
        .is_empty()
    );
    assert!(
        preview(
            format!("[{{\"a\":\"{}\"}}]", "x".repeat(249)).as_bytes(),
            &options
        )
        .await
        .is_err()
    );
    assert!(
        preview(b"[{\"a\":1,\"b\":2,\"c\":3}]", &options)
            .await
            .is_err()
    );
    let many = (0..20)
        .map(|n| format!("\"c{n}\":null"))
        .collect::<Vec<_>>()
        .join(",");
    assert!(
        preview(
            format!("[{{{many}}}]").as_bytes(),
            &TransferOptions {
                max_record_bytes: 1024,
                max_columns: 100,
                ..options
            }
        )
        .await
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn preview_cancels_while_scanning_unretained_array_whitespace() -> Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    file.write_all(b"[{\"a\":1},{\"a\":2},")?;
    let spaces = [b' '; 65536];
    for _ in 0..512 {
        file.write_all(&spaces)?;
    }
    file.write_all(b"{\"a\":3}]")?;
    let path = file.path().to_owned();
    let control = TransferControl::default();
    let cancel = control.clone();
    let mut progress = control.subscribe();
    let task = tokio::spawn(async move {
        preview_import(
            &path,
            TransferFormat::JsonArray,
            &TransferOptions {
                batch_rows: 1,
                ..Default::default()
            },
            &control,
        )
        .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        progress.wait_for(|p| p.rows >= 1),
    )
    .await??;
    cancel.cancel();
    let error = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await??
        .unwrap_err();
    ensure!(
        format!("{error:#}").contains("cancelled"),
        "Unexpected cancellation outcome: {error:#}"
    );
    Ok(())
}

#[test]
fn large_array_preview_and_oversized_sparse_record_have_bounded_memory() -> Result<()> {
    if std::env::var_os("ROWBOAT_PASS6_ARRAY_MEMORY_CHILD").is_none() {
        let status = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "large_array_preview_and_oversized_sparse_record_have_bounded_memory",
                "--nocapture",
            ])
            .env("ROWBOAT_PASS6_ARRAY_MEMORY_CHILD", "1")
            .status()?;
        ensure!(status.success(), "Memory subprocess failed");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("large.json");
    let mut file = std::io::BufWriter::new(std::fs::File::create(&path)?);
    file.write_all(b"[")?;
    let payload = "x".repeat(1100);
    for id in 0..65536 {
        write!(
            file,
            "{}{{\"id\":{id},\"payload\":\"{payload}\"}}",
            if id == 0 { "" } else { "," }
        )?;
    }
    file.write_all(b"]\n")?;
    file.flush()?;
    drop(file);
    let before = peak_kib()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(preview_import(
        &path,
        TransferFormat::JsonArray,
        &TransferOptions::default(),
        &TransferControl::default(),
    ))?;
    let growth = peak_kib()?.saturating_sub(before);
    assert_eq!(result.summary.rows, 65536);
    assert_eq!(result.sample.len(), 50);
    ensure!(result.summary.bytes > 64 * 1024 * 1024, "Fixture too small");
    ensure!(growth < 32 * 1024, "Preview RSS grew {growth} KiB");
    println!(
        "JSON array: {} bytes, {} rows, peak RSS growth {growth} KiB",
        result.summary.bytes, result.summary.rows
    );
    let mut sparse = std::fs::File::create(&path)?;
    sparse.write_all(b"[{\"a\":\"")?;
    sparse.set_len(512 * 1024 * 1024)?;
    let before = peak_kib()?;
    let error = runtime
        .block_on(preview_import(
            &path,
            TransferFormat::JsonArray,
            &TransferOptions {
                max_record_bytes: 256,
                ..Default::default()
            },
            &TransferControl::default(),
        ))
        .unwrap_err();
    ensure!(
        format!("{error:#}").contains("record exceeds"),
        "Wrong error: {error:#}"
    );
    ensure!(
        peak_kib()?.saturating_sub(before) < 8 * 1024,
        "Oversized record buffered before rejection"
    );
    Ok(())
}
