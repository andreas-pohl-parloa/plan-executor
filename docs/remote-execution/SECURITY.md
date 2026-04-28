# Remote Execution: Security Notes

This document captures the trust model behind plan-executor's remote
execution flow (`execute-plan.yml` + `plan-executor remote-setup`). It
exists separately from the workflow file so operators have somewhere to
read before granting an execution repo access to their target repos.

## GPG signing key reuse

`plan-executor remote-setup` provisions a passphraseless ed25519 GPG
signing key per user via the `find_ci_signing_key` flow:

- The key is generated locally and stored in `~/.gnupg/` on the machine
  that ran `remote-setup`. It has **no passphrase** so the GitHub
  Actions runner can use it non-interactively.
- Once generated, the same key is reused across every execution repo
  the same user provisions. This keeps commit identity stable across
  runs but means the key's blast radius scales with the number of
  execution repos that hold it as a secret.
- Anyone with filesystem access to `~/.gnupg/` on the workstation that
  ran `remote-setup` can extract the private key and sign commits as
  the user. Treat that machine like a hardware token: full-disk
  encryption, locked screen, no shared accounts.
- The key is also uploaded to each execution repo as the
  `GPG_SIGNING_KEY` secret (and `GPG_SIGNING_KEY_ID` for the key id).
  Repo admins can read the secret value during the brief window
  between `remote-setup` and the first run; afterwards GitHub masks it
  but admins can still overwrite or delete it. Restrict execution-repo
  admin membership accordingly.
- The key never appears in workflow logs — `Configure commit signing`
  imports it via stdin and GitHub redacts the secret automatically.
  Untrusted plans cannot exfiltrate it through the install step
  (the marketplace/plugin allow-list) or through `gh release download`
  (the checksum verification gate), but any code path that gets to run inside
  the runner has access to the same `gpg --export-secret-keys` that
  the trusted code uses. Reviewing every plan before merging the
  execution PR is the primary control.

### Rotation

There is no automated rotation flag yet. To rotate manually:

1. Delete the local key: `gpg --delete-secret-keys <KEY_ID>` then
   `gpg --delete-keys <KEY_ID>`.
2. Re-run `plan-executor remote-setup` for each affected execution
   repo. The setup flow will generate a new key and upload it as the
   new `GPG_SIGNING_KEY` secret, replacing the previous value.
3. (Optional) Revoke the old key on GitHub
   (`Settings → SSH and GPG keys`) so historical commits show as
   "Unverified" if the old key leaks.

If `remote-setup` later grows a `--rotate-gpg-key` flag, prefer that
path — it batches steps 1 and 2 atomically.
