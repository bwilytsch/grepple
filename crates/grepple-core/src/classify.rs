/// Command classification: assigns structured labels to session commands.
///
/// Labels fall into three categories by convention:
/// - **Activity**: `dev-server`, `build`, `test`, `lint`, `repl`, `infra`
/// - **Role**: `frontend`, `backend`
/// - **Framework**: `next`, `vite`, `webpack`, `turbo`, `flask`, `django`, `uvicorn`, `rails`, `modal`, `docker`, `nodemon`, `air`

/// Pattern-match a command string and return sorted, deduped labels.
pub fn classify_command(command: &str) -> Vec<String> {
    let lower = command.to_ascii_lowercase();
    let mut labels = Vec::new();

    if is_dev_server(&lower) {
        labels.push("dev-server".to_string());
    }
    if is_build(&lower) {
        labels.push("build".to_string());
    }
    if is_test(&lower) {
        labels.push("test".to_string());
    }
    if is_lint(&lower) {
        labels.push("lint".to_string());
    }
    if is_repl(&lower) {
        labels.push("repl".to_string());
    }
    if is_infra(&lower) {
        labels.push("infra".to_string());
    }
    if is_frontend(&lower) {
        labels.push("frontend".to_string());
    }
    if is_backend(&lower) {
        labels.push("backend".to_string());
    }

    labels.extend(detect_frameworks(&lower));

    labels.sort();
    labels.dedup();
    labels
}

/// Score labels against an intent string. Returns (bonus_points, reasons).
pub fn label_score(labels: &[String], intent: Option<&str>) -> (i64, Vec<String>) {
    let mut score = 0_i64;
    let mut reasons = Vec::new();

    if labels.contains(&"dev-server".to_string()) {
        score += 120;
        reasons.push("dev/runtime command".to_string());
    }

    let intent = intent.unwrap_or_default().to_ascii_lowercase();
    if intent.is_empty() {
        return (score, reasons);
    }

    if intent.contains("frontend") && labels.contains(&"frontend".to_string()) {
        score += 50;
        reasons.push("frontend intent match".to_string());
    }
    if intent.contains("backend") && labels.contains(&"backend".to_string()) {
        score += 50;
        reasons.push("backend intent match".to_string());
    }

    if [
        "error", "logs", "stack", "trace", "runtime", "server", "watch",
    ]
    .iter()
    .any(|needle| intent.contains(needle))
    {
        score += 25;
    }

    (score, reasons)
}

fn is_dev_server(lower: &str) -> bool {
    let patterns = [
        // JS/TS
        "modal serve",
        "pnpm dev",
        "pnpm start",
        "npm run dev",
        "npm run start",
        "yarn dev",
        "yarn start",
        "bun dev",
        "bun start",
        "vite",
        "next dev",
        "next start",
        "webpack-dev-server",
        "turbo dev",
        "nodemon",
        "remix dev",
        "nuxt dev",
        "astro dev",
        "svelte-kit dev",
        // Python
        "uvicorn",
        "flask run",
        "manage.py runserver",
        "gunicorn",
        "hypercorn",
        "fastapi",
        "litestar",
        // Ruby
        "rails server",
        "puma",
        // Go
        "air ",
        "go run ",
        // Java/Kotlin
        "mvn spring-boot:run",
        "gradle bootrun",
        "gradlew bootrun",
        "./gradlew bootrun",
        "quarkus dev",
        "micronaut:run",
        // PHP
        "php artisan serve",
        "php -s",
        "symfony serve",
        // .NET
        "dotnet run",
        "dotnet watch",
        // Elixir
        "mix phx.server",
        "iex -s mix",
        // Rust
        "cargo watch",
        "cargo run",
        // Infra
        "docker compose up",
        "docker-compose up",
    ];
    patterns.iter().any(|p| lower.contains(p))
}

fn is_frontend(lower: &str) -> bool {
    [
        "vite", "next", "webpack", "turbo", "frontend", "ui", "remix", "nuxt", "astro", "svelte",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn is_backend(lower: &str) -> bool {
    [
        "uvicorn",
        "flask",
        "django",
        "manage.py",
        "rails",
        "modal",
        "gunicorn",
        "hypercorn",
        "fastapi",
        "litestar",
        "puma",
        "spring-boot",
        "bootrun",
        "quarkus",
        "micronaut",
        "artisan serve",
        "symfony serve",
        "dotnet run",
        "dotnet watch",
        "phx.server",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn is_build(lower: &str) -> bool {
    [
        "cargo build",
        "npm run build",
        "yarn build",
        "pnpm build",
        "bun build",
        "tsc",
        "webpack ",
        "esbuild",
        "rollup",
        "turbo build",
        "go build",
        "mvn package",
        "mvn compile",
        "gradle build",
        "gradlew build",
        "dotnet build",
        "dotnet publish",
        "swift build",
        "mix compile",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn is_test(lower: &str) -> bool {
    [
        "cargo test",
        "cargo nextest",
        "pytest",
        "jest",
        "vitest",
        "npm test",
        "npm run test",
        "yarn test",
        "pnpm test",
        "bun test",
        "go test",
        "rspec",
        "minitest",
        "phpunit",
        "dotnet test",
        "swift test",
        "mix test",
        "gradle test",
        "mvn test",
        "playwright test",
        "cypress run",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn is_lint(lower: &str) -> bool {
    [
        "eslint",
        "prettier",
        "clippy",
        "ruff",
        "cargo fmt",
        "biome",
        "oxlint",
        "golangci-lint",
        "staticcheck",
        "rubocop",
        "phpstan",
        "phpcs",
        "swiftlint",
        "ktlint",
        "checkstyle",
        "mix format",
        "mix credo",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn is_repl(lower: &str) -> bool {
    // Match standalone REPL commands (at start or after whitespace)
    let repls = ["python", "node", "irb", "iex", "ghci", "lua"];
    for r in repls {
        if lower == r || lower.ends_with(&format!(" {r}")) {
            return true;
        }
    }
    false
}

fn is_infra(lower: &str) -> bool {
    ["docker", "k8s", "kubectl", "terraform", "pulumi"]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn detect_frameworks(lower: &str) -> Vec<String> {
    let mut frameworks = Vec::new();
    let mapping: &[(&[&str], &str)] = &[
        // JS/TS
        (&["next dev", "next start", "next "], "next"),
        (&["vite"], "vite"),
        (&["webpack"], "webpack"),
        (&["turbo"], "turbo"),
        (&["nodemon"], "nodemon"),
        (&["remix"], "remix"),
        (&["nuxt"], "nuxt"),
        (&["astro"], "astro"),
        (&["svelte"], "svelte"),
        // Python
        (&["flask"], "flask"),
        (&["django", "manage.py"], "django"),
        (&["uvicorn"], "uvicorn"),
        (&["gunicorn"], "gunicorn"),
        (&["fastapi"], "fastapi"),
        (&["litestar"], "litestar"),
        (&["modal"], "modal"),
        // Ruby
        (&["rails"], "rails"),
        (&["puma"], "puma"),
        // Go
        (&["air "], "air"),
        // Java/Kotlin
        (&["spring-boot", "bootrun"], "spring"),
        (&["quarkus"], "quarkus"),
        (&["micronaut"], "micronaut"),
        // PHP
        (&["artisan"], "laravel"),
        (&["symfony"], "symfony"),
        // .NET
        (&["dotnet run", "dotnet watch"], "dotnet"),
        // Elixir
        (&["phx.server", "phoenix"], "phoenix"),
        // Infra
        (&["docker"], "docker"),
    ];

    for (patterns, label) in mapping {
        if patterns.iter().any(|p| lower.contains(p)) {
            frameworks.push(label.to_string());
        }
    }

    frameworks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_next_dev() {
        let labels = classify_command("next dev");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"frontend".to_string()));
        assert!(labels.contains(&"next".to_string()));
    }

    #[test]
    fn classify_npm_run_dev() {
        let labels = classify_command("npm run dev");
        assert!(labels.contains(&"dev-server".to_string()));
    }

    #[test]
    fn classify_uvicorn() {
        let labels = classify_command("uvicorn app.main:app --reload");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"backend".to_string()));
        assert!(labels.contains(&"uvicorn".to_string()));
    }

    #[test]
    fn classify_flask_run() {
        let labels = classify_command("flask run --debug");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"backend".to_string()));
        assert!(labels.contains(&"flask".to_string()));
    }

    #[test]
    fn classify_vite() {
        let labels = classify_command("npx vite");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"frontend".to_string()));
        assert!(labels.contains(&"vite".to_string()));
    }

    #[test]
    fn classify_cargo_build() {
        let labels = classify_command("cargo build --release");
        assert!(labels.contains(&"build".to_string()));
        assert!(!labels.contains(&"dev-server".to_string()));
    }

    #[test]
    fn classify_cargo_test() {
        let labels = classify_command("cargo test --lib");
        assert!(labels.contains(&"test".to_string()));
        assert!(!labels.contains(&"dev-server".to_string()));
    }

    #[test]
    fn classify_jest() {
        let labels = classify_command("npx jest --watch");
        assert!(labels.contains(&"test".to_string()));
    }

    #[test]
    fn classify_eslint() {
        let labels = classify_command("eslint src/");
        assert!(labels.contains(&"lint".to_string()));
    }

    #[test]
    fn classify_docker_compose_up() {
        let labels = classify_command("docker compose up -d");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"infra".to_string()));
        assert!(labels.contains(&"docker".to_string()));
    }

    #[test]
    fn classify_rails_server() {
        let labels = classify_command("rails server -p 3000");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"backend".to_string()));
        assert!(labels.contains(&"rails".to_string()));
    }

    #[test]
    fn classify_turbo_dev() {
        let labels = classify_command("turbo dev");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"frontend".to_string()));
        assert!(labels.contains(&"turbo".to_string()));
    }

    #[test]
    fn classify_manage_py_runserver() {
        let labels = classify_command("python manage.py runserver");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"backend".to_string()));
        assert!(labels.contains(&"django".to_string()));
    }

    #[test]
    fn classify_modal_serve() {
        let labels = classify_command("modal serve app.py");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"backend".to_string()));
        assert!(labels.contains(&"modal".to_string()));
    }

    #[test]
    fn classify_nodemon() {
        let labels = classify_command("nodemon server.js");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"nodemon".to_string()));
    }

    #[test]
    fn classify_webpack_dev_server() {
        let labels = classify_command("webpack-dev-server --hot");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"frontend".to_string()));
        assert!(labels.contains(&"webpack".to_string()));
    }

    #[test]
    fn classify_empty_command() {
        let labels = classify_command("");
        assert!(labels.is_empty());
    }

    #[test]
    fn classify_unknown_command() {
        let labels = classify_command("echo hello");
        assert!(labels.is_empty());
    }

    #[test]
    fn labels_are_sorted_and_deduped() {
        let labels = classify_command("next dev");
        let mut sorted = labels.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(labels, sorted);
    }

    #[test]
    fn label_score_dev_server_bonus() {
        let labels = classify_command("next dev");
        let (score, reasons) = label_score(&labels, None);
        assert!(score >= 120);
        assert!(reasons.iter().any(|r| r.contains("dev/runtime")));
    }

    #[test]
    fn label_score_frontend_intent() {
        let labels = classify_command("next dev");
        let (score, reasons) = label_score(&labels, Some("frontend"));
        assert!(score >= 170); // 120 + 50
        assert!(reasons.iter().any(|r| r.contains("frontend intent")));
    }

    #[test]
    fn label_score_backend_intent() {
        let labels = classify_command("uvicorn app.main:app");
        let (score, reasons) = label_score(&labels, Some("backend"));
        assert!(score >= 170); // 120 + 50
        assert!(reasons.iter().any(|r| r.contains("backend intent")));
    }

    #[test]
    fn label_score_error_intent() {
        let labels = classify_command("next dev");
        let (score, _) = label_score(&labels, Some("error logs"));
        assert!(score >= 145); // 120 + 25
    }

    #[test]
    fn label_score_empty_labels() {
        let (score, reasons) = label_score(&[], Some("frontend"));
        assert_eq!(score, 0);
        assert!(reasons.is_empty());
    }

    #[test]
    fn classify_pnpm_dev() {
        let labels = classify_command("pnpm dev");
        assert!(labels.contains(&"dev-server".to_string()));
    }

    #[test]
    fn classify_yarn_start() {
        let labels = classify_command("yarn start");
        assert!(labels.contains(&"dev-server".to_string()));
    }

    #[test]
    fn classify_bun_dev() {
        let labels = classify_command("bun dev");
        assert!(labels.contains(&"dev-server".to_string()));
    }

    #[test]
    fn classify_air_go_hot_reload() {
        let labels = classify_command("air -c .air.toml");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"air".to_string()));
    }

    #[test]
    fn classify_ruff() {
        let labels = classify_command("ruff check .");
        assert!(labels.contains(&"lint".to_string()));
    }

    #[test]
    fn classify_clippy() {
        let labels = classify_command("cargo clippy -- -D warnings");
        assert!(labels.contains(&"lint".to_string()));
    }

    #[test]
    fn classify_vitest() {
        let labels = classify_command("vitest run");
        assert!(labels.contains(&"test".to_string()));
    }

    #[test]
    fn classify_pytest() {
        let labels = classify_command("pytest tests/ -v");
        assert!(labels.contains(&"test".to_string()));
    }

    #[test]
    fn classify_terraform() {
        let labels = classify_command("terraform apply");
        assert!(labels.contains(&"infra".to_string()));
    }

    #[test]
    fn classify_kubectl() {
        let labels = classify_command("kubectl get pods");
        assert!(labels.contains(&"infra".to_string()));
    }

    // --- Additional framework/runtime tests ---

    #[test]
    fn classify_spring_boot() {
        let labels = classify_command("mvn spring-boot:run");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"backend".to_string()));
        assert!(labels.contains(&"spring".to_string()));
    }

    #[test]
    fn classify_gradle_bootrun() {
        let labels = classify_command("./gradlew bootRun");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"backend".to_string()));
        assert!(labels.contains(&"spring".to_string()));
    }

    #[test]
    fn classify_quarkus_dev() {
        let labels = classify_command("quarkus dev");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"backend".to_string()));
        assert!(labels.contains(&"quarkus".to_string()));
    }

    #[test]
    fn classify_dotnet_run() {
        let labels = classify_command("dotnet run --project MyApi");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"backend".to_string()));
        assert!(labels.contains(&"dotnet".to_string()));
    }

    #[test]
    fn classify_dotnet_watch() {
        let labels = classify_command("dotnet watch run");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"backend".to_string()));
        assert!(labels.contains(&"dotnet".to_string()));
    }

    #[test]
    fn classify_php_artisan_serve() {
        let labels = classify_command("php artisan serve");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"backend".to_string()));
        assert!(labels.contains(&"laravel".to_string()));
    }

    #[test]
    fn classify_symfony_serve() {
        let labels = classify_command("symfony serve --no-tls");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"backend".to_string()));
        assert!(labels.contains(&"symfony".to_string()));
    }

    #[test]
    fn classify_phoenix_server() {
        let labels = classify_command("mix phx.server");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"backend".to_string()));
        assert!(labels.contains(&"phoenix".to_string()));
    }

    #[test]
    fn classify_gunicorn() {
        let labels = classify_command("gunicorn app:app --workers 4");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"backend".to_string()));
        assert!(labels.contains(&"gunicorn".to_string()));
    }

    #[test]
    fn classify_fastapi() {
        let labels = classify_command("fastapi dev main.py");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"backend".to_string()));
        assert!(labels.contains(&"fastapi".to_string()));
    }

    #[test]
    fn classify_remix_dev() {
        let labels = classify_command("remix dev");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"frontend".to_string()));
        assert!(labels.contains(&"remix".to_string()));
    }

    #[test]
    fn classify_nuxt_dev() {
        let labels = classify_command("nuxt dev");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"frontend".to_string()));
        assert!(labels.contains(&"nuxt".to_string()));
    }

    #[test]
    fn classify_astro_dev() {
        let labels = classify_command("astro dev");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"frontend".to_string()));
        assert!(labels.contains(&"astro".to_string()));
    }

    #[test]
    fn classify_cargo_watch() {
        let labels = classify_command("cargo watch -x run");
        assert!(labels.contains(&"dev-server".to_string()));
    }

    #[test]
    fn classify_go_run() {
        let labels = classify_command("go run ./cmd/server");
        assert!(labels.contains(&"dev-server".to_string()));
    }

    #[test]
    fn classify_go_build() {
        let labels = classify_command("go build -o bin/app .");
        assert!(labels.contains(&"build".to_string()));
    }

    #[test]
    fn classify_dotnet_build() {
        let labels = classify_command("dotnet build");
        assert!(labels.contains(&"build".to_string()));
    }

    #[test]
    fn classify_mvn_test() {
        let labels = classify_command("mvn test");
        assert!(labels.contains(&"test".to_string()));
    }

    #[test]
    fn classify_phpunit() {
        let labels = classify_command("phpunit tests/");
        assert!(labels.contains(&"test".to_string()));
    }

    #[test]
    fn classify_golangci_lint() {
        let labels = classify_command("golangci-lint run");
        assert!(labels.contains(&"lint".to_string()));
    }

    #[test]
    fn classify_puma() {
        let labels = classify_command("puma -C config/puma.rb");
        assert!(labels.contains(&"dev-server".to_string()));
        assert!(labels.contains(&"backend".to_string()));
        assert!(labels.contains(&"puma".to_string()));
    }

    #[test]
    fn classify_playwright_test() {
        let labels = classify_command("playwright test");
        assert!(labels.contains(&"test".to_string()));
    }

    #[test]
    fn classify_cypress_run() {
        let labels = classify_command("cypress run --spec tests/e2e");
        assert!(labels.contains(&"test".to_string()));
    }
}
