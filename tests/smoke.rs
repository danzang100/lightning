use clap::Parser;
use lightning::cli::{Cli, run};

#[test]
fn doctor_and_report_parse() {
    for sub in ["report", "doctor"] {
        Cli::try_parse_from(["lightning", sub]).unwrap();
    }
}

#[test]
fn ingest_requires_existing_root() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("l.db");
    let cli = Cli::try_parse_from([
        "lightning".as_ref(),
        "--db".as_ref(),
        db.as_os_str(),
        "ingest".as_ref(),
        "--root".as_ref(),
        dir.path().join("missing").as_os_str(),
    ])
    .unwrap();
    assert!(run(cli, &mut Vec::new()).is_err());
}
