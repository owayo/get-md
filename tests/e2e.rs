use std::process::Command;

fn get_md_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_get-md"))
}

#[test]
#[ignore] // Requires Chrome installed on the system
fn fetch_github_raw_readme() {
    let output = get_md_bin()
        .args([
            "https://raw.githubusercontent.com/owayo/get-md/refs/heads/main/README.md",
            "-q",
            "--no-cache",
        ])
        .output()
        .expect("Failed to execute get-md");

    assert!(
        output.status.success(),
        "get-md exited with error: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.is_empty(), "Output should not be empty");
    assert!(
        stdout.contains("get-md"),
        "Output should contain 'get-md': got:\n{stdout}",
    );
}
