//! Ingest-time classifiers. Raw text goes in, a class comes out, and the text is dropped.
//! Everything here is plain keyword matching so a label can always be explained.
use serde::Serialize;

pub const TASK_TYPES: [&str; 7] = ["debugging", "refactor", "feature", "test_writing", "exploration", "config_devops", "docs"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptClass {
    NewTask,
    Correction,
    Clarification,
    Approval,
    Abort,
    Continue,
}

impl PromptClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NewTask => "new_task",
            Self::Correction => "correction",
            Self::Clarification => "clarification",
            Self::Approval => "approval",
            Self::Abort => "abort",
            Self::Continue => "continue",
        }
    }
}

fn lower_words(text: &str) -> (String, Vec<String>) {
    let l = text.to_lowercase();
    let words = l
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect();
    (l, words)
}

const ABORT_MARKERS: [&str; 3] = ["[request interrupted", "request interrupted by user", "[interrupted"];
const CORRECTION_STARTS: [&str; 10] = ["no", "nope", "wrong", "stop", "undo", "revert", "actually", "wait", "don't", "dont"];
const CORRECTION_PHRASES: [&str; 14] = [
    "that's wrong",
    "thats wrong",
    "that is wrong",
    "not what i",
    "doesn't work",
    "does not work",
    "didn't work",
    "still fail",
    "still broken",
    "still not",
    "you broke",
    "that broke",
    "not working",
    "try again",
];
const APPROVAL_WORDS: [&str; 18] = [
    "yes", "yep", "yeah", "ok", "okay", "sure", "lgtm", "great", "thanks", "thank", "perfect", "nice", "proceed", "go", "continue", "approved", "good", "cool",
];

/// Classify one human prompt. `first` is true for the first prompt of a session.
pub fn prompt_class(text: &str, first: bool) -> PromptClass {
    let (l, words) = lower_words(text);
    let t = l.trim();
    if ABORT_MARKERS.iter().any(|m| t.starts_with(m)) {
        return PromptClass::Abort;
    }
    if words.is_empty() {
        return PromptClass::Continue;
    }
    let starts_correction = CORRECTION_STARTS.contains(&words[0].as_str()) && words.len() > 1;
    if !first && (starts_correction || CORRECTION_PHRASES.iter().any(|p| t.contains(p))) {
        return PromptClass::Correction;
    }
    if words.len() <= 6 && words.iter().all(|w| APPROVAL_WORDS.contains(&w.as_str()) || ["it", "do", "ahead", "that", "please", "looks", "lets", "let's", "with", "all"].contains(&w.as_str())) {
        return if first { PromptClass::NewTask } else { PromptClass::Approval };
    }
    if !first && words.len() <= 12 && t.ends_with('?') {
        return PromptClass::Clarification;
    }
    if first || words.len() >= 6 {
        return PromptClass::NewTask;
    }
    PromptClass::Continue
}

const HINTS: [(&str, &[&str]); 7] = [
    ("debugging", &["fix", "bug", "error", "errors", "failing", "fails", "failed", "crash", "crashes", "broken", "debug", "exception", "traceback", "panic", "regression", "issue", "wrong"]),
    ("refactor", &["refactor", "rename", "cleanup", "clean", "simplify", "restructure", "extract", "reorganize", "split", "dedupe", "tidy", "move"]),
    ("feature", &["add", "implement", "build", "create", "feature", "support", "new", "introduce", "endpoint", "command", "page"]),
    ("test_writing", &["test", "tests", "coverage", "unit", "spec", "fixture", "fixtures", "assert"]),
    ("exploration", &["explain", "understand", "why", "how", "what", "where", "investigate", "review", "look", "find", "analyze", "analyse", "compare", "research", "summarize"]),
    ("config_devops", &["deploy", "docker", "ci", "pipeline", "config", "configure", "install", "setup", "kubernetes", "workflow", "env", "release", "dependency", "dependencies", "version", "upgrade"]),
    ("docs", &["readme", "docs", "documentation", "document", "changelog", "comment", "comments", "docstring", "write-up", "guide"]),
];

/// Keyword scores per task type for one prompt. Failure words next to "test" count as debugging.
pub fn task_scores(text: &str) -> [u32; 7] {
    let (_, words) = lower_words(text);
    let mut s = [0u32; 7];
    for w in &words {
        for (i, (_, kws)) in HINTS.iter().enumerate() {
            if kws.contains(&w.as_str()) {
                s[i] += 1;
            }
        }
    }
    // "fix the failing tests" is debugging, not test writing.
    if s[0] > 0 && s[3] > 0 {
        s[3] = s[3].saturating_sub(s[0]);
    }
    s
}

/// Best task hint for one prompt, if any keyword matched.
pub fn task_hint(text: &str) -> Option<&'static str> {
    let s = task_scores(text);
    let (i, best) = s.iter().enumerate().max_by_key(|(i, v)| (**v, std::cmp::Reverse(*i)))?;
    (*best > 0).then(|| TASK_TYPES[i])
}

pub fn task_index(t: &str) -> Option<usize> {
    TASK_TYPES.iter().position(|x| *x == t)
}

/// Shell command class. Only the class is stored, never the command.
pub fn cmd_class(cmd: &str) -> &'static str {
    let l = cmd.to_lowercase();
    let has = |p: &str| l.contains(p);
    if has("git commit") {
        "git_commit"
    } else if has("git revert") || has("git reset --hard") || has("git checkout -- ") || has("git restore") || has("git stash") {
        "git_revert"
    } else if has("git push") {
        "git_push"
    } else if ["cargo test", "cargo nextest", "pytest", "npm test", "npm run test", "pnpm test", "yarn test", "jest", "vitest", "go test", "mvn test", "gradle test", "gradlew test", "dotnet test", "rspec", "phpunit", "python -m unittest", "bun test", "deno test", "ctest", "mix test"]
        .iter()
        .any(|p| has(p))
    {
        "test"
    } else if ["cargo clippy", "eslint", "ruff", "flake8", "mypy", "pylint", "golangci", "npm run lint", "pnpm lint", "cargo fmt --check", "prettier --check", "tsc --noemit"].iter().any(|p| has(p)) {
        "lint"
    } else if ["cargo build", "cargo check", "npm run build", "pnpm build", "yarn build", "tsc", "go build", "make", "mvn package", "mvn compile", "gradle build", "gradlew build", "dotnet build", "cmake --build", "vite build", "next build"]
        .iter()
        .any(|p| has(p))
    {
        "build"
    } else if ["npm install", "npm i ", "pnpm install", "yarn add", "pip install", "cargo add", "cargo install", "go get", "uv add", "poetry add"].iter().any(|p| has(p)) {
        "install"
    } else {
        "other"
    }
}

pub fn is_verification(cmd_class: &str) -> bool {
    matches!(cmd_class, "test" | "build" | "lint")
}

/// Canonical tool kind across harnesses.
pub fn tool_kind(name: &str) -> &'static str {
    let n = name.to_lowercase();
    let n = n.rsplit("__").next().unwrap_or(&n).to_string();
    match n.as_str() {
        "read" | "read_file" | "view" | "cat" | "notebookread" => "read",
        "write" | "edit" | "multiedit" | "notebookedit" | "apply_patch" | "patch" | "write_file" | "edit_file" | "str_replace" | "create_file" => "edit",
        "bash" | "powershell" | "shell" | "shell_command" | "exec_command" | "local_shell" | "run_shell" | "execute" | "unified_exec" => "shell",
        "grep" | "glob" | "list" | "ls" | "find" | "search" | "codesearch" | "list_dir" | "file_search" => "search",
        "webfetch" | "websearch" | "web_search" | "fetch" | "web_fetch" => "web",
        "task" | "agent" | "spawn_agent" | "delegate" => "task",
        _ => "other",
    }
}

/// Kind of file a path points at. Only the kind is stored.
pub fn target_kind(path: &str) -> &'static str {
    let p = format!("/{}", path.replace('\\', "/").to_lowercase());
    let name = p.rsplit('/').next().unwrap_or(&p);
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    if p.contains("/tests/") || p.contains("/test/") || p.contains("__tests__") || name.starts_with("test_") || name.contains("_test.") || name.contains(".test.") || name.contains(".spec.") || name.ends_with("_spec.rb") {
        "test"
    } else if matches!(ext, "md" | "mdx" | "rst" | "txt" | "adoc") || name == "license" {
        "doc"
    } else if matches!(ext, "toml" | "yaml" | "yml" | "json" | "ini" | "cfg" | "conf" | "env" | "lock" | "xml" | "properties" | "gradle")
        || name == "dockerfile"
        || name == "makefile"
        || name.starts_with(".")
        || p.contains(".github/")
    {
        "config"
    } else if ext.is_empty() {
        "other"
    } else {
        "code"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_classes() {
        assert_eq!(prompt_class("Add a --json flag to the report command", true), PromptClass::NewTask);
        assert_eq!(prompt_class("yes, go ahead", false), PromptClass::Approval);
        assert_eq!(prompt_class("No, that's not what I asked for", false), PromptClass::Correction);
        assert_eq!(prompt_class("the tests still fail after your change", false), PromptClass::Correction);
        assert_eq!(prompt_class("[Request interrupted by user]", false), PromptClass::Abort);
        assert_eq!(prompt_class("which file is it in?", false), PromptClass::Clarification);
        assert_eq!(prompt_class("Now write unit tests for the parser module please", false), PromptClass::NewTask);
        assert_eq!(prompt_class("hmm", false), PromptClass::Continue);
    }

    #[test]
    fn task_hints() {
        assert_eq!(task_hint("fix the failing tests in ingest"), Some("debugging"));
        assert_eq!(task_hint("write unit tests for the parser"), Some("test_writing"));
        assert_eq!(task_hint("refactor the store module and rename Acc"), Some("refactor"));
        assert_eq!(task_hint("explain how the report works"), Some("exploration"));
        assert_eq!(task_hint("update the README"), Some("docs"));
        assert_eq!(task_hint("set up the docker deploy pipeline"), Some("config_devops"));
        assert_eq!(task_hint("hello there"), None);
    }

    #[test]
    fn command_classes() {
        assert_eq!(cmd_class("cd x && cargo test --quiet"), "test");
        assert_eq!(cmd_class("cargo clippy --all-targets"), "lint");
        assert_eq!(cmd_class("cargo build --release"), "build");
        assert_eq!(cmd_class("git commit -m 'x'"), "git_commit");
        assert_eq!(cmd_class("git restore src/a.rs"), "git_revert");
        assert_eq!(cmd_class("ls -la"), "other");
    }

    #[test]
    fn tool_and_target_kinds() {
        assert_eq!(tool_kind("Read"), "read");
        assert_eq!(tool_kind("apply_patch"), "edit");
        assert_eq!(tool_kind("shell_command"), "shell");
        assert_eq!(tool_kind("mcp__x__grep"), "search");
        assert_eq!(target_kind("src/tests/a.rs"), "test");
        assert_eq!(target_kind("tests/e2e.rs"), "test");
        assert_eq!(target_kind("README.md"), "doc");
        assert_eq!(target_kind("Cargo.toml"), "config");
        assert_eq!(target_kind("src/lib.rs"), "code");
        assert_eq!(target_kind("web/app.test.ts"), "test");
    }
}
