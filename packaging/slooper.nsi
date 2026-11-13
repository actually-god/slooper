; slooper Windows installer (NSIS) — built by release.yml on
; windows-latest (NSIS ships with the runner image).
;
;   makensis -DSLOOPER_VER=<ver> -DSLOOPER_SRC=<staged-dir> packaging\slooper.nsi
;
; staged dir = the win job's stage\ tree (slooper.exe + README + LICENSE).
; Machine-wide install (elevation via RequestExecutionLevel admin), with
; uninstall registry entry, Start-menu group and an optional desktop
; shortcut. Unicode build; x64-only build refuses to install to
; PROGRAMFILES64 by construction (we never touch the 32-bit view).

Unicode true
!include MUI2.nsh

!ifndef SLOOPER_VER
  !define SLOOPER_VER "0.0.0"
!endif
; NOTE: makensis resolves relative paths against the SCRIPT's folder
; (this file lives in packaging\), not the caller's CWD.
!ifndef SLOOPER_SRC
  !define SLOOPER_SRC "..\stage"
!endif

Name "slooper ${SLOOPER_VER}"
OutFile "slooper-${SLOOPER_VER}-win64-setup.exe"
InstallDir "$PROGRAMFILES64\slooper"
InstallDirRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\slooper" "InstallLocation"
RequestExecutionLevel admin
ShowInstDetails show
ShowUnInstDetails show
SetCompressor /SOLID lzma

!define MUI_ABORTWARNING
!define MUI_ICON "..\assets\icon.ico"
!define MUI_UNICON "..\assets\icon.ico"

!insertmacro MUI_PAGE_LICENSE "${SLOOPER_SRC}\LICENSE"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN "$INSTDIR\slooper.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Launch slooper"
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; Version resource (shows in file properties + Programs panel)
VIAddVersionKey /LANG=1033 "ProductName"        "slooper"
VIAddVersionKey /LANG=1033 "FileDescription"    "slooper installer"
VIAddVersionKey /LANG=1033 "LegalCopyright"     "MIT License"
VIAddVersionKey /LANG=1033 "ProductVersion"     "${SLOOPER_VER}"
VIAddVersionKey /LANG=1033 "FileVersion"        "${SLOOPER_VER}"
VIProductVersion "${SLOOPER_VER}.0"

Var HaveDesktopShortcut

Section "slooper"
  SetShellVarContext all

  ; Old installs: kill leftovers first so the exe file is not locked.
  ; (TaskKill returns non-zero when nothing was running — ignore it.)
  nsExec::Exec 'taskkill /F /IM slooper.exe'
  Sleep 400

  SetOutPath "$INSTDIR"
  File "${SLOOPER_SRC}\slooper.exe"
  File "${SLOOPER_SRC}\README.md"
  File "${SLOOPER_SRC}\LICENSE"

  CreateDirectory "$SMPROGRAMS\slooper"
  CreateShortcut "$SMPROGRAMS\slooper\slooper.lnk" "$INSTDIR\slooper.exe" "" "$INSTDIR\slooper.exe" 0
  Delete "$SMPROGRAMS\slooper\Uninstall slooper.lnk" ; our .lnk name changed form
  CreateShortcut "$SMPROGRAMS\slooper\Uninstall.lnk" "$INSTDIR\uninstall.exe"

  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\slooper" "DisplayName" "slooper"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\slooper" "DisplayVersion" "${SLOOPER_VER}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\slooper" "Publisher" "actually-god"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\slooper" "URLInfoAbout" "https://github.com/actually-god/slooper"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\slooper" "DisplayIcon" "$INSTDIR\slooper.exe"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\slooper" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\slooper" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\slooper" "NoModify" 1
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\slooper" "NoRepair" 1

  WriteUninstaller "$INSTDIR\uninstall.exe"
SectionEnd

Section /o "Desktop shortcut"
  SetShellVarContext all
  CreateShortcut "$DESKTOP\slooper.lnk" "$INSTDIR\slooper.exe" "" "$INSTDIR\slooper.exe" 0
  StrCpy $HaveDesktopShortcut 1
SectionEnd

Section "Uninstall"
  SetShellVarContext all
  nsExec::Exec 'taskkill /F /IM slooper.exe'
  Sleep 400

  Delete "$INSTDIR\slooper.exe"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\slooper\slooper.lnk"
  Delete "$SMPROGRAMS\slooper\Uninstall.lnk"
  Delete "$SMPROGRAMS\slooper\Uninstall slooper.lnk"
  RMDir "$SMPROGRAMS\slooper"
  Delete "$DESKTOP\slooper.lnk"

  ; User data (config/projects/rack) is INTENTIONALLY kept — same
  ; principle as the Linux packages: uninstall removes programs, not work.
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\slooper"
SectionEnd
