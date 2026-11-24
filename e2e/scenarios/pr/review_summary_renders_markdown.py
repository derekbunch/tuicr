#!/usr/bin/env python3
# With `render_markdown = true`, a PR review summary (the top-level review body, here shaped like a Copilot review)
# renders as markdown: headings and emphasis lose their markers, and the HTML GitHub bots emit is reduced to its text
# (`<summary>` kept as a title, `<img>` as its alt text; comments, `<details>`, `<picture>` and `<sub>` dropped).
from e2e import Scenario, repos, scenario


@scenario
def main(s: Scenario) -> None:
    repos.write(s.dir / "config" / "tuicr" / "config.toml", "render_markdown = true\n")
    repo = repos.pr_repo(s.dir / "repo", origin="https://github.com/octo/demo.git")
    tui = s.start("pr", "1", cwd=repo, env={"E2E_VARIANT": "review_summary_html"})
    tui.wait_for("copilot-pull-request-reviewer", 20)
    tui.wait_for(r"│  Changes recommended")  # heading without `###`
    tui.expect("The setup fails without a virtual environment.")
    tui.expect(r"│  3 open issues")
    tui.expect("• medium First issue")
    tui.expect(r"│  Review effort: Balanced")
    tui.reject(r"ccr-overview|<details|<summary|<picture|<img|<sub>|\*\*fails\*\*|###")
    tui.quit()
