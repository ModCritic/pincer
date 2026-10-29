# Legacy Windows Sandbox runner

This runner exists only for local smoke checks and must not be treated as the
canonical Windows desktop test entrypoint.

Run it from the repository root on a Windows host with Windows Sandbox enabled:

```powershell
.\tests\runners\windows-sandbox\run-tests-in-sandbox.ps1
.\tests\runners\windows-sandbox\run-tests-in-sandbox.ps1 harness_wpf
```

The host script builds selected Rust test binaries and Windows fixtures, maps
the repository root into the sandbox as `C:\pincer`, and streams logs from the
inside-sandbox runner.
