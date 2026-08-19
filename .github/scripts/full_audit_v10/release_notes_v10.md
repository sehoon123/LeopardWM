Personal-fork prerelease from `sehoon123/LeopardWM`.

Full repository hardening pass:
- independent distribution identity and exact `sehoon.10` version reporting
- corrected custom prerelease update checks with numeric revision ordering
- serialized Settings saves, visible update preference, future-field preservation, responsive GUI, and Edge smoke tests
- shared atomic daemon/CLI persistence
- IPC startup readiness and token-buffer alignment hardening
- correct Win32 message-loop error handling and graceful worker startup
- raw thumbnail-handle ownership cleanup and taskbar worker readiness
- stale focus-border cleanup and consistent monitor isolation across windows, borders, tab strips, taskbar state, and transitions
- floating-window lifecycle cleanup and structural edge-centering preservation
- verified Scoop manifest, MSI, and portable ZIP

This release is published only from the user's independent repository.
