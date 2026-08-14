//! Repository gates.
//!
//! These check the properties that reviews miss and that no unit test can
//! express, because they are about the *shape* of the workspace rather than the
//! behaviour of any one function.
//!
//! Run with `cargo xtask architecture`.
//!
//! Every gate here is written so that losing sight of the code is a failure
//! rather than a pass. A gate that scans an empty set reports success, which
//! looks exactly like a gate that checked everything and found nothing wrong —
//! so each one states how much it expected to inspect and fails if it inspected
//! less.
#![forbid(unsafe_code)]

use std::{fs, path::Path, process::ExitCode};

fn main() -> ExitCode {
    let command = std::env::args().nth(1).unwrap_or_default();
    let root = workspace_root();

    let failures = match command.as_str() {
        "architecture" => architecture(&root),
        "" | "help" | "--help" => {
            println!("usage: cargo xtask <architecture>");
            return ExitCode::SUCCESS;
        }
        other => {
            eprintln!("unknown task: {other}");
            return ExitCode::from(2);
        }
    };

    if failures.is_empty() {
        println!("architecture: all gates pass");
        return ExitCode::SUCCESS;
    }
    for failure in &failures {
        eprintln!("architecture: {failure}");
    }
    eprintln!("\n{} gate(s) failed", failures.len());
    ExitCode::from(1)
}

/// The workspace root.
///
/// Derived from the manifest directory rather than the executable: the binary
/// lives at `<root>/target/<profile>/xtask`, but Cargo supplies
/// `<root>/xtask` directly.
fn workspace_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the workspace root")
        .to_path_buf()
}

fn architecture(root: &Path) -> Vec<String> {
    let mut failures = Vec::new();
    failures.extend(core_is_platform_free(root));
    failures.extend(platform_selection_uses_cfg(root));
    failures.extend(unsafe_is_confined(root));
    failures.extend(dependencies_are_pinned(root));
    failures.extend(member_manifests_use_the_workspace_table(root));
    failures.extend(secrets_are_redacted_in_one_place(root));
    failures.extend(tests_are_the_tail_of_their_file(root));
    failures.extend(live_tests_cannot_run_by_accident(root));
    failures
}

/// Reports a gate that found nothing to look at.
///
/// Every gate below scans a directory or a manifest section that is known to be
/// non-empty today. If one comes back empty, the code moved and the gate did
/// not follow — and a silent pass is the worst of the three outcomes, because
/// it is indistinguishable from a real one.
fn inspected_nothing(gate: &str, what: &str, count: usize) -> Option<String> {
    (count == 0).then(|| {
        format!(
            "{gate}: found no {what} to inspect, so it is no longer proving anything. \
             Point it at where the code moved to."
        )
    })
}

/// `desktop-core` must build and test on any platform, so it may not name a
/// platform crate or a platform dependency.
///
/// Checked in the source and in the manifest, because the two fail differently.
/// A path reference is caught by matching `name::` rather than the bare word,
/// so the name appearing in prose does not trip it. A dependency merely
/// *declared* compiles nothing yet but still has to build wherever core builds,
/// so it breaks `cargo test -p desktop-core` on another platform before any
/// code uses it — and no amount of scanning source would show it.
fn core_is_platform_free(root: &Path) -> Vec<String> {
    const FORBIDDEN: [&str; 8] = [
        "desktop_linux",
        "desktop_macos",
        "atspi",
        "ashpd",
        "x11rb",
        "objc2",
        "zbus",
        "libc",
    ];

    let mut failures = Vec::new();
    let sources = rust_sources(&root.join("crates/desktop-core"));
    failures.extend(inspected_nothing(
        "core_is_platform_free",
        "desktop-core sources",
        sources.len(),
    ));

    for (path, source) in sources {
        for name in FORBIDDEN {
            let needle = format!("{name}::");
            if source
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .any(|line| line.contains(&needle))
            {
                failures.push(format!(
                    "{}: desktop-core must stay platform-independent but references `{name}`",
                    relative(root, &path)
                ));
            }
        }
    }

    let manifest_path = root.join("crates/desktop-core/Cargo.toml");
    let Ok(manifest) = fs::read_to_string(&manifest_path) else {
        failures.push(format!("cannot read {}", relative(root, &manifest_path)));
        return failures;
    };
    for line in manifest_dependency_lines(&manifest) {
        let declared = line.split_whitespace().next().unwrap_or(&line);
        let name = declared.trim_end_matches(".workspace").replace('-', "_");
        if FORBIDDEN.contains(&name.as_str()) {
            failures.push(format!(
                "crates/desktop-core/Cargo.toml: depends on `{name}`, which does not build \
                 everywhere desktop-core has to"
            ));
        }
    }
    failures
}

/// The dependency lines of a manifest, from every section that declares one.
///
/// Section headers, blanks and comments are dropped; a `[target.…]` block
/// counts, because a platform-gated dependency is still a dependency.
fn manifest_dependency_lines(manifest: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut in_dependencies = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_dependencies = trimmed.ends_with("dependencies]");
            continue;
        }
        if in_dependencies && !trimmed.is_empty() && !trimmed.starts_with('#') {
            lines.push(trimmed.to_owned());
        }
    }
    lines
}

/// Platform backends are chosen by target `cfg`, never by a Cargo feature.
///
/// A feature can be enabled on the wrong platform, and the failure would be a
/// link error at best and a silently wrong backend at worst, so a
/// `feature = "linux"`-style gate is rejected outright.
fn platform_selection_uses_cfg(root: &Path) -> Vec<String> {
    let mut failures = Vec::new();

    let cli_manifest = root.join("crates/desktop-cli/Cargo.toml");
    let Ok(manifest) = fs::read_to_string(&cli_manifest) else {
        return vec![format!("cannot read {}", relative(root, &cli_manifest))];
    };

    for (crate_name, target_os) in [("desktop-linux", "linux"), ("desktop-macos", "macos")] {
        let expected = format!("[target.'cfg(target_os = \"{target_os}\")'.dependencies]");
        if !manifest.contains(&expected) {
            failures.push(format!(
                "crates/desktop-cli/Cargo.toml: {crate_name} must be selected by \
                 `{expected}`, not by a feature"
            ));
        }
    }

    for (path, source) in rust_sources(&root.join("crates")) {
        for suspicious in ["feature = \"linux\"", "feature = \"macos\""] {
            if source.contains(suspicious) {
                failures.push(format!(
                    "{}: platform selection must use cfg(target_os), found `{suspicious}`",
                    relative(root, &path)
                ));
            }
        }
    }
    failures
}

/// `unsafe` is allowed only in the platform crates, and only in the modules
/// that actually wrap a C API.
///
/// This list must match the modules `desktop-macos` grants `allow(unsafe_code)`
/// to. That crate denies the lint and re-grants it per module, so the compiler
/// is the primary enforcement; this gate is the second one, and catches a
/// module being granted the lint without anyone revisiting the boundary.
fn unsafe_is_confined(root: &Path) -> Vec<String> {
    const ALLOWED: [&str; 4] = [
        "crates/desktop-macos/src/ax.rs",
        "crates/desktop-macos/src/capture.rs",
        "crates/desktop-macos/src/input.rs",
        "crates/desktop-macos/src/process.rs",
    ];

    /// Assembled rather than written out, so this file does not match itself
    /// now that the scan covers `xtask` too.
    const BLOCK: &str = concat!("unsafe", " {");
    const FUNCTION: &str = concat!("unsafe", " fn");

    let mut failures = Vec::new();
    let mut sources = rust_sources(&root.join("crates"));
    sources.extend(rust_sources(&root.join("xtask")));
    failures.extend(inspected_nothing(
        "unsafe_is_confined",
        "Rust sources",
        sources.len(),
    ));

    for (path, source) in sources {
        let relative_path = relative(root, &path);
        if ALLOWED.contains(&relative_path.as_str()) {
            continue;
        }
        let offending = source
            .lines()
            .enumerate()
            .filter(|(_, line)| {
                let trimmed = line.trim_start();
                !trimmed.starts_with("//")
                    && !trimmed.starts_with("#![")
                    && (trimmed.contains(BLOCK) || trimmed.starts_with(FUNCTION))
            })
            .map(|(index, _)| index + 1)
            .collect::<Vec<_>>();
        if !offending.is_empty() {
            failures.push(format!(
                "{relative_path}: unsafe is confined to the platform FFI modules, \
                 found at line(s) {offending:?}"
            ));
        }
    }
    failures
}

/// Every third-party dependency is pinned exactly, so a `cargo update` cannot
/// change behaviour without a reviewed diff.
/// Every third-party dependency is pinned exactly. Path dependencies are the
/// workspace's own crates and are exempt.
fn dependencies_are_pinned(root: &Path) -> Vec<String> {
    let manifest_path = root.join("Cargo.toml");
    let Ok(manifest) = fs::read_to_string(&manifest_path) else {
        return vec!["cannot read the workspace Cargo.toml".to_owned()];
    };

    let mut failures = Vec::new();
    let mut in_dependencies = false;
    let mut checked = 0;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_dependencies = trimmed == "[workspace.dependencies]";
            continue;
        }
        if !in_dependencies || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.contains("path = ") {
            continue;
        }
        checked += 1;
        if !trimmed.contains("version = \"=") {
            let name = trimmed.split_whitespace().next().unwrap_or(trimmed);
            failures.push(format!(
                "Cargo.toml: dependency `{name}` must be pinned with `version = \"=x.y.z\"`"
            ));
        }
        if !trimmed.contains("default-features = false") {
            let name = trimmed.split_whitespace().next().unwrap_or(trimmed);
            failures.push(format!(
                "Cargo.toml: dependency `{name}` must set `default-features = false`"
            ));
        }
    }
    failures.extend(inspected_nothing(
        "dependencies_are_pinned",
        "third-party dependencies in [workspace.dependencies]",
        checked,
    ));
    failures
}

/// Members declare dependencies as `foo.workspace = true`, never with a version
/// of their own.
///
/// Otherwise [`dependencies_are_pinned`] is checking a table the dependency
/// never passes through: a `foo = "1.0"` written straight into a member
/// manifest takes a caret range and the crate's default features, and both
/// gates above it read only the workspace table and see nothing.
fn member_manifests_use_the_workspace_table(root: &Path) -> Vec<String> {
    let mut failures = Vec::new();
    let mut checked = 0;

    let mut manifests: Vec<std::path::PathBuf> = fs::read_dir(root.join("crates"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join("Cargo.toml"))
        .filter(|path| path.is_file())
        .collect();
    manifests.push(root.join("xtask/Cargo.toml"));
    manifests.sort();

    for manifest_path in manifests {
        let Ok(manifest) = fs::read_to_string(&manifest_path) else {
            failures.push(format!("cannot read {}", relative(root, &manifest_path)));
            continue;
        };
        for line in manifest_dependency_lines(&manifest) {
            checked += 1;
            if line.contains("workspace = true") || line.contains("path = ") {
                continue;
            }
            let name = line.split_whitespace().next().unwrap_or(&line);
            failures.push(format!(
                "{}: dependency `{name}` must be declared as `{name}.workspace = true` so the \
                 exact pin and the feature list stay in one reviewed place",
                relative(root, &manifest_path)
            ));
        }
    }

    failures.extend(inspected_nothing(
        "member_manifests_use_the_workspace_table",
        "member dependency declarations",
        checked,
    ));
    failures
}

/// Redaction has exactly one implementation.
///
/// Two would eventually disagree, and the direction that fails open leaks a
/// password. The normalizer is where every snapshot path converges, so that is
/// where the decision lives.
/// Redaction is not one place, it is one rule: anything that puts an element's
/// value in front of a caller must ask whether that value is a secret first.
/// Three sites do — the snapshot normalizer and the two renderers behind
/// `desktop inspect` — and the failure mode is a fourth appearing that forgets.
///
/// Reading `.value` is not by itself a problem: a platform adapter reads it to
/// build a node, and whether that node is secret is decided by its role. What
/// matters is emitting it, so the check is scoped to files that render,
/// serialize, or construct the user-facing `Element`.
fn secrets_are_redacted_in_one_place(root: &Path) -> Vec<String> {
    let mut failures = Vec::new();
    let mut checked = 0;

    for (path, source) in rust_sources(&root.join("crates")) {
        let relative_path = relative(root, &path);
        if relative_path.contains("testing.rs") {
            continue;
        }
        let code = production_code(&source);

        let reads_value = code.contains("node.value") || code.contains("element.value");
        let emits = code.contains("sink.") || code.contains("json!(") || code.contains("Element {");
        if !(reads_value && emits) {
            continue;
        }

        checked += 1;
        if !code.contains("is_secure()") {
            failures.push(format!(
                "{relative_path}: emits an element's value without consulting \
                 `is_secure()`. A password field must never reach output; see \
                 the snapshot normalizer for the shape."
            ));
        }
    }

    // A gate that checks nothing passes silently, which is the one outcome
    // worse than failing.
    if checked < 2 {
        failures.push(format!(
            "the redaction gate found only {checked} output path(s) to check; it has \
             drifted from the code and is no longer proving anything"
        ));
    }
    failures
}

/// The part of a file that is compiled into the shipped binary.
///
/// Everything up to the first `#[cfg(test)]`, which is sound only because
/// [`tests_are_the_tail_of_their_file`] holds.
fn production_code(source: &str) -> String {
    source
        .lines()
        .take_while(|line| !line.trim_start().starts_with("#[cfg(test)]"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The part of a file that exists only under `cfg(test)`.
///
/// Either all of it, when the file carries an inner `#![cfg(test)]`, or
/// everything from the first `#[cfg(test)]` item onwards.
fn test_code(source: &str) -> String {
    if source
        .lines()
        .any(|line| line.trim_start().starts_with("#![cfg(test)]"))
    {
        return source.to_owned();
    }
    source
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("#[cfg(test)]"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A file's tests are one `mod tests` at the end of it, and nothing follows.
///
/// [`production_code`] and [`test_code`] both split a file at its first
/// `#[cfg(test)]`, so production code written after a test module would be
/// invisible to every gate that reads the first half — including the redaction
/// one, where invisible means a password field could reach output unchecked.
/// The convention holds everywhere today; this is what keeps it true.
fn tests_are_the_tail_of_their_file(root: &Path) -> Vec<String> {
    let mut failures = Vec::new();
    let mut checked = 0;

    for (path, source) in rust_sources(&root.join("crates")) {
        let lines: Vec<&str> = source.lines().collect();
        let Some(start) = lines
            .iter()
            .position(|line| line.trim_start().starts_with("#[cfg(test)]"))
        else {
            continue;
        };
        checked += 1;
        let relative_path = relative(root, &path);

        let opens_the_module = lines[start + 1..]
            .iter()
            .find(|line| !line.trim().is_empty())
            .is_some_and(|line| line.trim_start().starts_with("mod tests"));
        if !opens_the_module {
            failures.push(format!(
                "{relative_path}: the first `#[cfg(test)]`, at line {}, must open the file's \
                 `mod tests`; the gates split every file there and would read the wrong half",
                start + 1
            ));
            continue;
        }

        let mut depth: i64 = 0;
        let mut opened = false;
        for (offset, line) in lines[start + 1..].iter().enumerate() {
            depth += line.matches('{').count() as i64 - line.matches('}').count() as i64;
            opened |= line.contains('{');
            if !opened || depth > 0 {
                continue;
            }
            if let Some(trailing) = lines[start + offset + 2..]
                .iter()
                .find(|rest| !rest.trim().is_empty())
            {
                failures.push(format!(
                    "{relative_path}: `{}` follows the tests module, so it is invisible to \
                     every gate that reads a file's production half. Move it above the tests.",
                    trailing.trim()
                ));
            }
            break;
        }
    }

    failures.extend(inspected_nothing(
        "tests_are_the_tail_of_their_file",
        "files with tests",
        checked,
    ));
    failures
}

/// A test may drive a real desktop only where that is both obvious and refused
/// by default.
///
/// `live.rs` launches an application, and an application appears on whatever
/// display the test process inherited. Run from an ordinary terminal that is
/// the user's screen: the window opens over their work and takes the keystrokes
/// they were in the middle of typing. `Calculator::launch` refuses unless the
/// display belongs to an agent session, and this keeps the two properties that
/// refusal rests on — that no test spawns a process anywhere else, and that
/// nothing in that file runs without `--ignored`.
fn live_tests_cannot_run_by_accident(root: &Path) -> Vec<String> {
    let mut failures = Vec::new();
    let mut tests = 0;

    for (path, source) in rust_sources(&root.join("crates")) {
        let relative_path = relative(root, &path);
        let is_live = path.file_name().is_some_and(|name| name == "live.rs");

        if !is_live && test_code(&source).contains("Command::new") {
            failures.push(format!(
                "{relative_path}: a test that spawns a process belongs in live.rs, which is \
                 ignored by default and refuses a display it does not own"
            ));
        }
        if !is_live {
            continue;
        }

        let lines: Vec<&str> = source.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            if line.trim() != "#[test]" {
                continue;
            }
            tests += 1;
            let ignored = lines[index + 1..]
                .iter()
                .take_while(|rest| !rest.trim_start().starts_with("fn "))
                .any(|rest| rest.trim_start().starts_with("#[ignore"));
            if !ignored {
                failures.push(format!(
                    "{relative_path}: the test at line {} is not `#[ignore]`d, so a plain \
                     `cargo test` would launch an application on somebody's desktop",
                    index + 1
                ));
            }
        }
    }

    failures.extend(inspected_nothing(
        "live_tests_cannot_run_by_accident",
        "live tests",
        tests,
    ));
    failures
}

fn rust_sources(directory: &Path) -> Vec<(std::path::PathBuf, String)> {
    let mut out = Vec::new();
    collect(directory, &mut out);
    out
}

fn collect(directory: &Path, out: &mut Vec<(std::path::PathBuf, String)>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            collect(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs")
            && let Ok(source) = fs::read_to_string(&path)
        {
            out.push((path, source));
        }
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
