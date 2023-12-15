// Copyright 2024 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

use crate::common::TestEnvironment;
use crate::common::TestWorkDir;

#[test]
fn test_run_simple() {
    let mut test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let fake_formatter = assert_cmd::cargo::cargo_bin("fake-formatter");
    assert!(fake_formatter.is_file());
    let fake_formatter_path = fake_formatter.to_string_lossy().into_owned();
    test_env.add_paths_to_normalize(fake_formatter.clone(), "$FAKE_FORMATTER_PATH");
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("A.txt", "A");
    work_dir.run_jj(&["commit", "-m", "A"]).success();
    work_dir.write_file("b.txt", "b");
    work_dir.run_jj(&["commit", "-m", "B"]).success();
    work_dir.write_file("c.txt", "test to replace");
    work_dir.run_jj(&["commit", "-m", "C"]).success();
    insta::assert_snapshot!(get_log_output(&work_dir), @r"
    @  zsuskulnrvyrovkzqrwmxqlsskqntxvp
    ○  kkmpptxzrspxrzommnulwmwkkqwworplC
    │
    ○  rlvkpnrzqnoowoytxnquwvuryrwnrmlpB
    │
    ○  qpvuntsmwlqtpsluzzsnyyzlmlwvmlnuA
    │
    ◆  zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz
    [EOF]
    ");
    // `--tee touched.txt` creates a file in each working copy, so every commit's
    // tree gets rewritten.
    let stdout = work_dir
        .run_jj(&[
            "run",
            &format!("{fake_formatter_path} --stdout x --tee touched.txt"),
            "-r",
            "..@",
        ])
        .success()
        .stdout;
    insta::assert_snapshot!(stdout, @"xxxx[EOF]");
}

// This tests a simple `jj run 'cargo fmt' invocation on the repo. It is based
// on the git-branchless demo here: https://github.com/arxanas/git-branchless/wiki/Command:-git-test
#[test]
fn test_run_simple_with_cargo() {
    // The test environment clears env vars; cargo (via rustup) needs
    // RUSTUP_HOME to find the active toolchain. Skip when it isn't set.
    let Some(rustup_home) = std::env::var_os("RUSTUP_HOME") else {
        eprintln!("Skipping test_run_simple_with_cargo: RUSTUP_HOME is not set");
        return;
    };
    let mut test_env = TestEnvironment::default();
    test_env.add_env_var("RUSTUP_HOME", rustup_home);
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    // A minimal Cargo project layout so `cargo fmt` discovers the sources.
    work_dir.write_file(
        "Cargo.toml",
        indoc::indoc! {r#"
            [package]
            name = "demo"
            version = "0.0.0"
            edition = "2021"
        "#},
    );
    work_dir.write_file(
        "src/main.rs",
        indoc::indoc! {r#"

                mod foo;


                fn main() {
                    println!("{output}", output = foo::bar());

                }
        "#},
    );
    work_dir.write_file(
        "src/foo.rs",
        indoc::indoc! {r#"
              pub fn bar() -> String {
                    "bart".to_owned()
              }

        "#},
    );

    work_dir.run_jj(["commit", "-m", "Initial repo"]).success();
    work_dir
        .run_jj(["run", "cargo fmt", "-r", "root()..@"])
        .success();
    let output = work_dir
        .run_jj(["file", "show", "-r", "@-", "src/main.rs"])
        .success();
    // src/main.rs should be nicely formatted now.
    insta::assert_snapshot!(output.stdout, @r#"
    mod foo;

    fn main() {
        println!("{output}", output = foo::bar());
    }
    [EOF]
    "#);
}

#[test]
fn test_run_on_immutable() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    let fake_formatter = assert_cmd::cargo::cargo_bin("fake-formatter");
    assert!(fake_formatter.is_file());
    let fake_formatter_path = fake_formatter.to_string_lossy();
    work_dir.write_file("A.txt", "A");
    work_dir.run_jj(&["commit", "-m", "A"]).success();
    work_dir.write_file("b.txt", "b");
    work_dir.run_jj(&["commit", "-m", "B"]).success();
    work_dir.write_file("c.txt", "test to replace");
    work_dir.run_jj(&["commit", "-m", "C"]).success();
    insta::assert_snapshot!(get_log_output(&work_dir), @r"
    @  zsuskulnrvyrovkzqrwmxqlsskqntxvp
    ○  kkmpptxzrspxrzommnulwmwkkqwworplC
    │
    ○  rlvkpnrzqnoowoytxnquwvuryrwnrmlpB
    │
    ○  qpvuntsmwlqtpsluzzsnyyzlmlwvmlnuA
    │
    ◆  zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz
    [EOF]
    ");
    let output = work_dir
        .run_jj(&[
            "run",
            &format!("{fake_formatter_path} --uppercase"),
            "-r",
            "root()", // Running on the root commit is nonsensical.
        ])
        .success();
    insta::assert_snapshot!(output.stderr, @"");
    insta::assert_snapshot!(output.stdout, @"");
}

#[test]
fn test_run_noop() {
    let mut test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let fake_formatter = assert_cmd::cargo::cargo_bin("fake-formatter");
    assert!(fake_formatter.is_file());
    let fake_formatter_path = fake_formatter.to_string_lossy().into_owned();
    test_env.add_paths_to_normalize(fake_formatter.clone(), "$FAKE_FORMATTER_PATH");
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("A.txt", "A");
    work_dir.run_jj(&["commit", "-m", "A"]).success();
    work_dir.write_file("b.txt", "b");
    work_dir.run_jj(&["commit", "-m", "B"]).success();
    work_dir.write_file("c.txt", "test to replace");
    work_dir.run_jj(&["commit", "-m", "C"]).success();
    insta::assert_snapshot!(get_log_output(&work_dir), @r"
    @  zsuskulnrvyrovkzqrwmxqlsskqntxvp
    ○  kkmpptxzrspxrzommnulwmwkkqwworplC
    │
    ○  rlvkpnrzqnoowoytxnquwvuryrwnrmlpB
    │
    ○  qpvuntsmwlqtpsluzzsnyyzlmlwvmlnuA
    │
    ◆  zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz
    [EOF]
    ");
    // `--stdout foo` writes to the subprocess's stdout, which `jj run` buffers
    // and emits to its own stdout. No tracked files in the working copy change,
    // so no commits get rewritten. Using a fixed string keeps the per-commit
    // output identical, so the concatenated stdout is stable regardless of the
    // (non-deterministic) order in which the parallel jobs finish.
    let output = work_dir
        .run_jj(&[
            "run",
            &format!("{fake_formatter_path} --stdout foo"),
            "-r",
            "..@",
        ])
        .success();
    insta::assert_snapshot!(output.stdout, @"foofoofoofoo[EOF]");
    insta::assert_snapshot!(output.stderr, @r"
    No commits were rewritten as the command did not modify any tracked files
    Nothing changed.
    [EOF]
    ");
}

#[test]
fn test_run_sets_env_vars() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("seed.txt", "seed");
    work_dir.run_jj(&["commit", "-m", "seed"]).success();

    // Show the change_id and commit_id so the reader can match them against
    // the values the subprocess writes into the per-commit working copy.
    let log_template = r#"change_id ++ " " ++ commit_id ++ " " ++ description ++ "\n""#;
    insta::assert_snapshot!(
        work_dir.run_jj(&["log", "-T", log_template]),
        @r"
    @  rlvkpnrzqnoowoytxnquwvuryrwnrmlp fc4c875c9bc90128cbb9e8084dd5f5f336b383d9
    ○  qpvuntsmwlqtpsluzzsnyyzlmlwvmlnu 5fbe90560fed1c39d46a46a672ba98abd53bdc6d seed
    │
    ◆  zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz 0000000000000000000000000000000000000000
    [EOF]
    "
    );

    // Each subprocess echoes its JJ_CHANGE_ID and JJ_COMMIT_ID into files in
    // the per-commit working copy, modifying the tree so the commit gets
    // rewritten with those files.
    work_dir
        .run_jj(&[
            "run",
            "echo $JJ_CHANGE_ID > change_id.txt && echo $JJ_COMMIT_ID > commit_id.txt",
            "-r",
            "@-",
        ])
        .success();

    insta::assert_snapshot!(
        work_dir.run_jj(&["file", "show", "-r", "@-", "change_id.txt"]),
        @r"
    qpvuntsmwlqtpsluzzsnyyzlmlwvmlnu
    [EOF]
    "
    );
    insta::assert_snapshot!(
        work_dir.run_jj(&["file", "show", "-r", "@-", "commit_id.txt"]),
        @r"
    5fbe90560fed1c39d46a46a672ba98abd53bdc6d
    [EOF]
    "
    );
}

#[test]
fn test_run_from_subdir_skips_commits_without_it() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");

    // First commit has only root-level files; no `sub/` exists yet.
    work_dir.write_file("seed.txt", "seed");
    work_dir.run_jj(&["commit", "-m", "no-sub"]).success();
    // Second commit adds `sub/file.txt`, so `sub/` exists from here on.
    work_dir.write_file("sub/file.txt", "x");
    work_dir.run_jj(&["commit", "-m", "with-sub"]).success();

    // Run from inside sub/ on both ancestors. The command creates `ran.txt`
    // in cwd, so we can later tell where it ran. The `no-sub` commit has no
    // `sub/` directory and should be skipped; the `with-sub` commit has
    // `sub/` and should be rewritten with `sub/ran.txt` added.
    let sub_dir = work_dir.dir("sub");
    let output = sub_dir
        .run_jj(&["run", "touch ran.txt", "-r", "@-|@--"])
        .success()
        .normalize_backslash();
    insta::assert_snapshot!(output.stderr, @r"
    Skipped commit 3bb1f1ca3c09a8e6be46ef48515803464b16b426: directory does not exist: sub
    Rewrote 1 commits with touch ran.txt
    Working copy  (@) now at: kkmpptxz 3548431a (empty) (no description set)
    Parent commit (@-)      : rlvkpnrz 3aa9a235 with-sub
    Added 1 files, modified 0 files, removed 0 files
    [EOF]
    ");

    // The rewritten `with-sub` commit has `sub/ran.txt`, alongside the
    // pre-existing `sub/file.txt`.
    insta::assert_snapshot!(
        work_dir
            .run_jj(&["file", "list", "-r", "@-"])
            .normalize_backslash(),
        @r"
    seed.txt
    sub/file.txt
    sub/ran.txt
    [EOF]
    "
    );
}

#[test]
fn test_run_root_flag() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("sub/file.txt", "x");
    work_dir.run_jj(&["commit", "-m", "with-sub"]).success();

    // Invoke `jj run` from inside sub/, but pass `--root` so the command
    // executes from the workspace root and `ran.txt` lands at the top level.
    let sub_dir = work_dir.dir("sub");
    sub_dir
        .run_jj(&["run", "--root", "touch ran.txt", "-r", "@-"])
        .success();

    insta::assert_snapshot!(
        work_dir
            .run_jj(&["file", "list", "-r", "@-"])
            .normalize_backslash(),
        @r"
    ran.txt
    sub/file.txt
    [EOF]
    "
    );
}

#[test]
fn test_run_failure_rewrites_nothing() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("A.txt", "A");
    work_dir.run_jj(&["commit", "-m", "A"]).success();
    work_dir.write_file("b.txt", "b");
    work_dir.run_jj(&["commit", "-m", "B"]).success();
    let log_before = get_log_output(&work_dir);
    insta::assert_snapshot!(log_before, @r"
    @  kkmpptxzrspxrzommnulwmwkkqwworpl
    ○  rlvkpnrzqnoowoytxnquwvuryrwnrmlpB
    │
    ○  qpvuntsmwlqtpsluzzsnyyzlmlwvmlnuA
    │
    ◆  zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz
    [EOF]
    ");

    // Fail on commit B; succeed (modify the tree) on every other commit. If
    // any subprocess fails, `jj run` must roll back: no commit gets rewritten,
    // even the ones whose commands ran to completion before B's failure
    // propagated.
    let cmd = "if [ \"$JJ_CHANGE_ID\" = 'rlvkpnrzqnoowoytxnquwvuryrwnrmlp' ]; then exit 1; fi; \
               touch ran.txt";
    let output = work_dir.run_jj(&["run", cmd, "-r", "..@"]);
    assert!(!output.status.success(), "expected `jj run` to fail");

    // Log is unchanged: same change_ids, same shape, no descendants of B got
    // rebased onto a new commit.
    assert_eq!(get_log_output(&work_dir), log_before);
}

#[test]
fn test_run_recovers_after_failure() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("A.txt", "A");
    work_dir.run_jj(&["commit", "-m", "A"]).success();
    work_dir.write_file("b.txt", "b");
    work_dir.run_jj(&["commit", "-m", "B"]).success();

    // First run fails outright on every commit, leaving the per-commit
    // working copies in `.jj/run/default/` behind.
    let first = work_dir.run_jj(&["run", "exit 1", "-r", "..@"]);
    assert!(!first.status.success(), "expected first `jj run` to fail");

    // A second run with a working command must succeed despite those leftover
    // directories — `jj run` clears each per-commit dir before reusing it.
    work_dir
        .run_jj(&["run", "touch ran.txt", "-r", "..@"])
        .success();

    // Both commits in `..@` now carry `ran.txt`.
    insta::assert_snapshot!(
        work_dir.run_jj(&["file", "list", "-r", "@-"]),
        @r"
    A.txt
    b.txt
    ran.txt
    [EOF]
    "
    );
    insta::assert_snapshot!(
        work_dir.run_jj(&["file", "list", "-r", "@--"]),
        @r"
    A.txt
    ran.txt
    [EOF]
    "
    );
}

fn get_log_output(work_dir: &TestWorkDir) -> String {
    work_dir
        .run_jj(&["log", "-T", r#"change_id ++ description ++ "\n""#])
        .success()
        .stdout
        .to_string()
}
