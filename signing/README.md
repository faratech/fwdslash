# Code signing (Azure Trusted Signing)

`sign.ps1` signs `.exe`/`.msi`/`.msix` artifacts through Azure Trusted Signing
(account `fara-codesigning`, certificate profile `MikeFara`, Public Trust) and
verifies each signature. `common.ps1` locates signtool and the
`Azure.CodeSigning` dlib and loads credentials; `install-dlib.ps1` downloads
the dlib from nuget.org into `lib\` (gitignored — CI runs it fresh each time).

Auth comes from the environment — on GitHub Actions the three secrets:

    AZURE_TENANT_ID
    AZURE_CLIENT_ID
    AZURE_CLIENT_SECRET

Environment variables win over the optional `.env.codesigning` file, so CI
needs no file at all. `.env.codesigning` (real secret) is gitignored and must
never be committed; `.env.codesigning.example` is the template.

Vendored from the WindowsForum code signing kit (`/mnt/c/code/sign`); the
signing reference values — account, profile, tenant, client ID, timestamp URL —
live in `metadata.json` and `common.ps1`, never in the workflow.
