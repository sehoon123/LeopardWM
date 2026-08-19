# SetWindowRgn v10 recovery plan

This control branch exists only to build and verify the first real SetWindowRgn release from `main` commit `bfb2bb1409f39e29fa3530d3858fe6fab733d0c7`.

The product source is kept on a separate clean branch and reaches `main` only after the Windows test, Clippy, release-build, MSI administrative-install, archive-integrity, and published-asset identity gates pass.
