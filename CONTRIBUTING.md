# Contributing to AS Help MCP Server

Thank you for your interest in contributing! This document provides guidelines for contributors.

## Getting Started

### Prerequisites

- [Rust 1.85+](https://www.rust-lang.org/tools/install)
- B&R Automation Studio Help files (for integration testing)

### Development Setup

1. **Clone the repository:**

   ```bash
   git clone https://github.com/BRDK-Public/as-help-mcp.git
   cd as-help-mcp
   ```

2. **Build:**

   ```bash
   cargo build
   ```

3. **Run tests:**

   ```bash
   cargo test
   ```

## Making Changes

### Branch Naming

- `feature/add-new-search-filter`
- `fix/search-query-escaping`
- `docs/update-installation-guide`
- `refactor/simplify-indexer`

### Commit Messages

Follow conventional commit format:

```
type(scope): description

[optional body]
```

Types: `feat`, `fix`, `docs`, `style`, `refactor`, `test`, `chore`

Examples:

- `feat(search): add category filtering to search_help`
- `fix(indexer): handle missing HelpID gracefully`

## Pull Request Process

1. Create a feature branch from `main`
2. Make your changes with appropriate tests
3. Run locally:
   ```bash
   cargo fmt --check
   cargo clippy -- -D warnings
   cargo test
   ```
4. Push your branch and create a Pull Request
5. Wait for CI checks to pass

### PR Requirements

- All CI checks must pass (fmt, clippy, tests)
- At least one maintainer approval required
- No merge conflicts with `main`

## Coding Standards

- Run `cargo fmt` before committing
- Fix all `cargo clippy` warnings
- Use `anyhow` for application errors, `thiserror` for library errors
- Add tests for new functionality

## Testing

```bash
cargo test              # Run all tests
cargo test -- --nocapture  # Show println output
```

## Reporting Issues

Use our [GitHub issue tracker](https://github.com/BRDK-Public/as-help-mcp/issues).

## Security

If you discover a security vulnerability, please follow our [Security Policy](SECURITY.md) and report it privately.
