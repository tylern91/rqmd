# Accessibility

rqmd is a command-line tool and an MCP server, with its documentation in Markdown in this repository.
There is no graphical interface and no website. We want people who use screen readers, braille displays,
text-only terminals or other assistive technology to be able to run rqmd, read its output and contribute
to it.

This document says what we aim for, which environments we support, what we know falls short, and how to
report a barrier. It makes no claim of conformance to any standard, and nobody has formally evaluated
rqmd's accessibility yet.

## What we aim for

- **Output that does not depend on a terminal's tricks.** Escape sequences are written only to a
  terminal. Piped or redirected output, on stdout and on stderr, is plain text.
- **Colour you can turn off.** Set `NO_COLOR` (to any value) and styling is off on a terminal.
- **Progress you can read line by line.** Set `TERM=dumb` and progress prints as plain lines, each ending
  in a newline, instead of redrawing one line in place. `rqmd query` reports its stages on stderr the same
  way and never on stdout.
- **Output that other tools can read.** `--format json`, `csv`, `md`, `xml` and `files` give structured or
  plain results, so you can pass them to a tool you already use. Ranking and scores do not depend on
  whether progress is shown.
- **File names cannot inject escape codes.** File and folder names are untrusted text, and we keep
  control characters in them out of the lines rqmd prints (for example in `rqmd status`).

The details and the full table are in [Colour and progress output](docs/CLI.md#colour-and-progress-output).

## Supported environments

rqmd runs on macOS arm64 and Linux x86_64 (see [INSTALL.md](docs/INSTALL.md)).

We have **not** tested rqmd with specific screen readers (such as VoiceOver, NVDA or Orca) or braille
displays, and we do not want to imply support we have not checked. If you use one, your report of what
works and what does not is useful.

## Known limitations

- **Live progress repaints one line by default.** On a terminal, `update`, `embed` and `collection add`
  redraw a single line in place. Some screen readers may announce that badly. Workaround: run with
  `TERM=dumb` for plain lines.
- **The architecture diagram has no text description inside the file.** `assets/rqmd_architecture.svg` has
  no embedded title or description. The README gives the image alt text, and
  [ARCHITECTURE.md](docs/ARCHITECTURE.md) describes the same pipeline in text.
- **No assistive-technology testing.** See "Supported environments".

## Reporting a barrier

If something in rqmd's output, commands or documentation gets in your way, please
[open an issue](https://github.com/tylern91/rqmd/issues/new?template=bug_report.md). The bug-report
template works, and you do not need to fill in every section. It helps if you can say:

- what you were trying to do and what happened instead
- the command you ran, and the terminal, OS and assistive technology you use (versions if you know them)
- the output of `rqmd doctor`, if it is relevant

You do not need to share a diagnosis or any personal information, and screenshots or recordings are
optional.

### What to expect

rqmd has one maintainer, so we cannot promise fixed deadlines. We will:

- acknowledge the report and treat it as useful expertise, not a complaint
- explain the next step, a workaround if there is one, and anything that blocks a fix
- tell you when the plan changes
- ask you to confirm a fix before closing the issue, where that makes sense

## Contributing

Accessibility is owned by the maintainer, [@tylern91](https://github.com/tylern91). When you change what
rqmd prints, please:

- keep the output readable without colour, and keep escape sequences off anything that is not a terminal
- check the change with `NO_COLOR=1` and with `TERM=dumb`
- keep new output on stdout plain and parseable, and put status and progress on stderr
- in documentation, keep a logical heading order, use descriptive link text (not "click here"), and give
  every image alt text

## Feedback

Accessibility is ongoing work. If you have suggestions for this document or for our practices, open an
issue or a pull request.
