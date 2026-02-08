# Assistant CLI

TUI client for the [Assistant](https://github.com/kcosr/assistant) workspace, built with [FrankenTUI](https://github.com/Dicklesworthstone/frankentui).

## Features

- **Lists Browser**: Browse and manage lists with items
  - Real-time search (scoped to selected list or global)
  - AQL query support with column selection
  - Click column headers to sort
  - WebSocket updates for real-time sync
  - Vim keybindings (j/k navigation)
  - Mouse support (click, scroll, double-click)
  - OSC 52 clipboard (works over SSH)

## Installation

```bash
cargo install --path .
```

## Usage

```bash
# Set the Assistant API URL
export ASSISTANT_URL=http://localhost:3000

# Optional: Filter lists by name (comma-separated substrings)
export LISTS_FILTER=work,personal

# Run the lists browser
lists
```

## Keybindings

### Global
| Key | Action |
|-----|--------|
| `q` | Quit |
| `Tab` | Switch focus (sidebar ↔ table) |
| `Esc` | Clear filter/selection, go back |

### Sidebar (Lists)
| Key | Action |
|-----|--------|
| `/` | Filter lists by name |
| `j`/`k` or `↑`/`↓` | Navigate lists |
| `Enter` | Open selected list |

### Table (Items)
| Key | Action |
|-----|--------|
| `/` | Search items |
| `:` | Enter AQL query mode |
| `n` | Add new item |
| `d` | Delete selected item |
| `Space` | Toggle completed status |
| `t` | Move item to top |
| `b` | Move item to bottom |
| `o` | Open URL in browser |
| `y` | Copy URL/title to clipboard |
| `Enter` | View item details |
| `j`/`k` or `↑`/`↓` | Navigate items |

### Input Modes
| Key | Action |
|-----|--------|
| `Enter` | Confirm/execute |
| `Esc` | Cancel and clear |
| `Ctrl+C` | Clear input (if any) or exit mode |
| `↑`/`↓` | Navigate while filtering |

## AQL Queries

Use `:` to enter AQL mode. Examples:

```
status != completed order by updated desc
show title, tags, notes
tags contains work
```

Click column headers to sort (updates AQL automatically).

## Environment Variables

| Variable | Description |
|----------|-------------|
| `ASSISTANT_URL` | Base URL for Assistant API (required) |
| `LISTS_FILTER` | Comma-separated list name filters |

## Requirements

- Rust nightly (uses 2024 edition features)
- Terminal with mouse support
- Assistant server running

## License

MIT
