#!/usr/bin/env python3
# With `render_markdown = true`, a saved comment shows rendered markdown: emphasis markers are dropped, list items get
# bullets, a table is laid out in columns and a GitHub alert shows its title. The editor still shows the source.
import json

from e2e import Scenario, repos, scenario

BODY = """Use **bold** here
- first item
- second item

| Key | Value |
| --- | ----- |
| a   | 1     |

> [!WARNING]
> Mind the gap"""


@scenario
def main(s: Scenario) -> None:
    repos.write(s.dir / "config" / "tuicr" / "config.toml", "render_markdown = true\n")
    tui = s.diff()
    repo = s.dir / "repo"
    active = [x["slug"] for x in json.loads(s.cli("review", "list", "--repo", repo)) if x.get("active")]
    assert active, f"no active session in review list: {s.cli('review', 'list', '--all')}"
    s.cli("review", "add", "--repo", repo, "--session", active[0], "--target-file", "README.md", "--line", "4", BODY)

    tui.wait_for("Use bold here", 15)  # picked up by the review watcher; markers gone
    tui.reject(r"\*\*bold\*\*")
    tui.expect("• first item")
    tui.expect(r"Key │ Value")
    tui.expect(r"a   │ 1")
    tui.expect("Warning")
    tui.expect(r"│ Mind the gap")
    tui.reject(r"\[!WARNING\]")

    # The editor edits source, so it shows the markers the rendered view hides.
    tui.keys("g").keys("m")
    tui.keys("i").wait_for("^ COMMENT ")
    tui.expect(r"Use \*\*bold\*\* here")
    tui.expect(r"\[!WARNING\]")
    tui.keys("C-c").wait_for("^ NORMAL ")
    tui.quit()
