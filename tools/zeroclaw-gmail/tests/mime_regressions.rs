//! Synthetic MIME regression fixtures. This file can be copied to
//! tools/zeroclaw-gmail/tests/mime_regressions.rs without production changes.
use anyhow::Result;
use zeroclaw_gmail::{model::read_file, operations::parse_content, store::Store};

fn message(headers: &str, body: &str) -> Vec<u8> {
    format!(
        "From: owner@example.com\r\n\
         To: recipient@example.net\r\n\
         Subject: MIME fixture\r\n\
         Message-ID: <fixture@example.com>\r\n\
         MIME-Version: 1.0\r\n\
         {headers}\r\n{body}"
    )
    .into_bytes()
}

fn assert_not_editable(raw: &[u8]) -> Result<()> {
    let store = Store::memory()?;
    if let Ok((_, editable)) = parse_content(&store, raw, None) {
        assert!(
            !editable,
            "unsafe MIME must be rejected or marked non-editable"
        );
    }
    Ok(())
}

fn signed_body() -> &'static str {
    "--signed-boundary\r\n\
     Content-Type: text/plain; charset=utf-8\r\n\r\n\
     Signed body\r\n\
     --signed-boundary\r\n\
     Content-Type: application/pgp-signature; name=signature.asc\r\n\
     Content-Disposition: attachment; filename=signature.asc\r\n\r\n\
     synthetic-signature\r\n\
     --signed-boundary--\r\n"
}

fn multipart_with_attachment(attachment_headers: &str) -> Vec<u8> {
    message(
        "Content-Type: multipart/mixed; boundary=mixed-boundary\r\n",
        &format!(
            "--mixed-boundary\r\n\
             Content-Type: text/plain\r\n\r\n\
             Plain body\r\n\
             --mixed-boundary\r\n\
             {attachment_headers}\r\n\
             aGVsbG8=\r\n\
             --mixed-boundary--\r\n"
        ),
    )
}

#[test]
fn ordinary_plain_and_multipart_mime_remain_editable() -> Result<()> {
    let fixtures = [
        message("Content-Type: text/plain\r\n", "Plain body"),
        multipart_with_attachment(
            "Content-Type: application/octet-stream\r\n\
             Content-Disposition: attachment; filename=fixture.bin\r\n\
             Content-Transfer-Encoding: base64\r\n",
        ),
    ];
    for raw in fixtures {
        let store = Store::memory()?;
        let (_, editable) = parse_content(&store, &raw, None)?;
        assert!(editable, "ordinary fixture must be supported");
    }
    Ok(())
}

#[test]
fn signed_mime_is_not_editable() -> Result<()> {
    assert_not_editable(&message(
        "Content-Type: multipart/signed; boundary=signed-boundary; protocol=\"application/pgp-signature\"\r\n",
        signed_body(),
    ))
}

#[test]
fn nested_signed_mime_is_not_editable() -> Result<()> {
    assert_not_editable(&message(
        "Content-Type: multipart/mixed; boundary=outer-boundary\r\n",
        &format!(
            "--outer-boundary\r\n\
             Content-Type: multipart/signed; boundary=signed-boundary; protocol=\"application/pgp-signature\"\r\n\r\n\
             {}\r\n\
             --outer-boundary--\r\n",
            signed_body()
        ),
    ))
}

#[test]
fn duplicate_root_content_type_is_not_editable() -> Result<()> {
    assert_not_editable(&message(
        "Content-Type: text/plain\r\n\
         Content-Type: text/html\r\n",
        "<b>hello</b>",
    ))
}

#[test]
fn duplicate_root_transfer_encoding_is_not_editable() -> Result<()> {
    assert_not_editable(&message(
        "Content-Type: text/plain\r\n\
         Content-Transfer-Encoding: 7bit\r\n\
         Content-Transfer-Encoding: base64\r\n",
        "aGVsbG8=",
    ))
}

#[test]
fn duplicate_part_content_type_is_not_editable() -> Result<()> {
    assert_not_editable(&multipart_with_attachment(
        "Content-Type: text/plain\r\n\
         Content-Type: application/octet-stream\r\n\
         Content-Disposition: attachment; filename=fixture.bin\r\n\
         Content-Transfer-Encoding: base64\r\n",
    ))
}

#[test]
fn duplicate_part_transfer_encoding_is_not_editable() -> Result<()> {
    assert_not_editable(&multipart_with_attachment(
        "Content-Type: application/octet-stream\r\n\
         Content-Disposition: attachment; filename=fixture.bin\r\n\
         Content-Transfer-Encoding: 7bit\r\n\
         Content-Transfer-Encoding: base64\r\n",
    ))
}

#[test]
fn duplicate_part_content_disposition_is_not_editable() -> Result<()> {
    assert_not_editable(&multipart_with_attachment(
        "Content-Type: application/octet-stream\r\n\
         Content-Disposition: inline; filename=first.bin\r\n\
         Content-Disposition: attachment; filename=second.bin\r\n\
         Content-Transfer-Encoding: base64\r\n",
    ))
}

#[cfg(unix)]
#[test]
fn fifo_attachment_is_rejected_promptly() -> Result<()> {
    use std::time::{Duration, Instant};
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().canonicalize()?;
    let fifo = root.join("input.fifo");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()?
            .success()
    );
    let start = Instant::now();
    assert!(read_file(fifo.to_str().expect("synthetic UTF-8 path"), &[root]).is_err());
    assert!(start.elapsed() < Duration::from_secs(1));
    Ok(())
}

#[test]
fn encrypted_multipart_is_not_editable() -> Result<()> {
    assert_not_editable(&message(
        "Content-Type: multipart/encrypted; boundary=encrypted-boundary; protocol=\"application/pgp-encrypted\"\r\n",
        "--encrypted-boundary\r\n\
         Content-Type: application/pgp-encrypted; name=control.asc\r\n\
         Content-Disposition: attachment; filename=control.asc\r\n\r\n\
         Version: 1\r\n\
         --encrypted-boundary\r\n\
         Content-Type: application/octet-stream; name=encrypted.asc\r\n\
         Content-Disposition: attachment; filename=encrypted.asc\r\n\r\n\
         synthetic-ciphertext\r\n\
         --encrypted-boundary--\r\n",
    ))
}

#[test]
fn smime_envelope_is_not_editable() -> Result<()> {
    for subtype in ["pkcs7-mime", "x-pkcs7-mime"] {
        assert_not_editable(&message(
            &format!(
                "Content-Type: application/{subtype}; smime-type=enveloped-data; name=smime.p7m\r\n\
                 Content-Disposition: attachment; filename=smime.p7m\r\n\
                 Content-Transfer-Encoding: base64\r\n"
            ),
            "aGVsbG8=",
        ))?;
    }
    Ok(())
}
