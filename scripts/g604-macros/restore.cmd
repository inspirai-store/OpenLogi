@echo off
setlocal
set OPENLOGI_PROFILE=dev
"%~dp0g604-recovery.exe" restore
pause
