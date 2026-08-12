# Why is this directory named `workflows.disabled`?

These are the repository's GitHub Actions workflows (the upstream CI/build
pipelines plus a `check-web` job for the web deployment mode). They were
pushed from an environment whose GitHub App credential lacks the
`workflows` permission, which GitHub requires to create files under
`.github/workflows/`.

To activate CI, rename the directory back:

```bash
git mv .github/workflows.disabled .github/workflows
git commit -m "Enable GitHub Actions workflows"
git push
```
