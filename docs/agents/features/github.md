# GitHub Repositories

Read this before you change the GitHub Repositories section, or anything else
that reads from GitHub.

- The section reads the signed-in account through `gh` on the machine pilus
  runs on, never through a daemon or SSH.
- It shows three repositories: those in `Settings::github_pinned` first, then
  the ones pushed to most recently.
- Every list, and each repository's issues, pull requests and workflow runs,
  goes through `pilus_client::GhAccount`'s cache:
  - lists that are still fresh cost no request;
  - REST lists revalidate by `ETag`;
  - a rate limit is waited out and persisted in `pilus.db`.

  Do not call `gh` for these from the GUI directly, or the cache and its limits
  are bypassed.
- `Settings::github` and `github_repositories` switch the feature off. Off
  means no requests at all.
- The `visual-test` captures fill the feed with samples, so a capture never
  reaches GitHub.
