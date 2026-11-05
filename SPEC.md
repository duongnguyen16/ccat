# Spec: `ccat` — Claude Code CLI Profile Manager & Launcher

Fast, minimal, and keyboard-driven launcher and profile manager built with Rust and `ratatui` for Claude Code CLI.

---

## 1. Terminal Viewport Architecture

- **Launcher Screen (Normal Mode):** Rendered **inline** directly inside the terminal flow (`Viewport::Inline(20)`), maintaining command-line context without entering full-screen alternate buffer. All section steps are rendered contiguously with clean vertical connectors (`│`).
- **Manager Screen (`Manage profiles`):** Switches dynamically to the **Full-Screen Alternate Buffer** (`EnterAlternateScreen`), providing the full split-pane management environment (profile details, forms, live API model picker, delete confirmation, keyboard shortcuts).
- **Clean Path Normalization:** Paths on Windows are automatically cleaned to strip verbatim UNC prefixes (`\\?\`) using `dunce`.

---

## 2. Interaction Flow

```
[Launch ccat] ──▶ (Inline Viewport)
   │
   ├── ● Working folder:
   │    │   [✓] D:\Dev\ccat
   │    │   [ ] D:\Dev\my-project
   │    │   [ ] Browse folder... (OS Native Folder Dialog via rfd)
   │    │
   ├── ● Select profile:
   │    │   [✓] dev (http://localhost:20128/v1)
   │    │   [ ] staging
   │    │   [ ] Original Claude Code (standard environment)
   │    │   [ ] Manage profiles... ────────────────────────────┐
   │    │                                                      ▼
   └── ● Auto mode: [● On] [○ Off]                    [Full-Screen Manager]
        │                                             - Split-pane profile list & detail
        │ [Enter]                                     - ProfileForm create/edit
        ▼                                             - ModelPicker (/v1/models)
   [Spawn claude & Exit ccat] ◀───────────────────────┘ (Esc returns to inline launcher)
```

---

## 3. Inline Launcher UI

```
● Working folder:  D:\Dev\ccat
│   [✓] D:\Dev\ccat
│   [ ] D:\Dev\another-project
│   [ ] Browse folder... (Open OS File Dialog)
│
● Select profile:
│   [✓] dev (default)  — http://localhost:20128/v1
│   [ ] local-ollama   — http://localhost:11434/v1
│   [ ] Original Claude Code (standard environment)
│   [ ] Manage profiles... (Open full TUI manager)
│
● Auto mode:   [● On]   [○ Off]   (--dangerously-skip-permissions enabled)
```

---

## 4. Key Features

1. **Continuous Tree Connector:** Solid, uninterrupted vertical `│` progress lines with zero layout gaps or misalignments.
2. **No Emojis & Clean Typography:** Emojis have been replaced with standard ASCII/Unicode brackets and text (`[+]`, `[Custom]`, `[ok]`).
3. **No Shortcut Clutter in Launcher:** The launcher UI is streamlined and uncluttered; shortcut keys are only displayed in the management screen.
4. **UNC Prefix Cleanup:** Windows `\\?\` paths are normalized to clean drive letter paths (`D:\...`).
5. **Selection Persistence:** Working folder, selected profile, and auto mode setting are remembered across sessions in `~/.ccat.json`.
6. **OS Native Folder Picker:** Integrated with `rfd` and `dunce`.
