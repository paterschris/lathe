# Mobile development with Lathe

Lathe ships a workflow for building React Native / Expo apps and installing them on an Android device, with no Android Studio install required. Phase 1 (this doc) covers the manual setup and the cheapest path for common workflows. A future first-run wizard (Phase 3, planned) will automate the toolchain install.

## Pick your workflow

| Goal                                                                     | Best path           | Local toolchain?       | Iteration speed |
| ------------------------------------------------------------------------ | ------------------- | ---------------------- | --------------- |
| Active dev: edit JS, see it on the phone instantly                       | **Debug + Metro**   | Yes                    | seconds         |
| Test persistence / behavior **without your laptop attached**             | **EAS preview**     | No (cloud build)       | ~10 min / build |
| Production-style local build (no internet, no EAS credits)               | **Local release**   | Yes (plus keystore)    | 1 to 2 min      |

Important: debug builds bundle a `__DEV__` JS payload that **expects to reach the Metro dev server on launch**. They will not work standalone. For "install on phone, close laptop, walk away," you need a release variant. EAS preview is the lowest-friction route because it builds in the cloud, signs the APK, and avoids the entire local Android SDK install.

## Debug + Metro (active dev)

This is the everyday command while you're iterating.

1. Phone on same wifi (or USB) with developer mode and ADB enabled.
2. `npx expo run:android --device` (provided as the `android: run on device (debug)` task).
3. Edit JS. Fast Refresh handles the rest.

The debug APK keeps a connection to Metro running on your laptop. Close Metro or unplug, and the app shows the red "could not connect to development server" screen.

## EAS preview (walk-away test build)

For "install once, test for hours / days without my laptop," this is the path. No local Android SDK needed.

```sh
npm install -g eas-cli
eas login
eas build:configure                          # one-time, writes eas.json
eas build --platform android --profile preview --non-interactive
```

EAS builds in the cloud (free tier covers a few builds per month), produces a signed APK, and gives you a download link. Tap-install on the phone, no ADB needed. AsyncStorage and other persistence work identically to a Play Store install.

If your `eas.json` doesn't already have a `preview` profile, the bare minimum is:

```json
{
  "build": {
    "preview": {
      "distribution": "internal",
      "android": { "buildType": "apk" }
    },
    "production": {}
  }
}
```

Tradeoff: each build is ~10 min queued, and you burn EAS credits. Fine for periodic verification, painful for many builds per day.

## Local release build (no internet, no EAS)

Faster than EAS, no credits, but you maintain the local toolchain and a release keystore.

See "Toolchain" below for the install. Once set up, generate a release keystore once (more detail in "Release keystore" below) and run the `android: build release APK` task.

## Toolchain (only needed for local builds)

Required components for an Expo 54 build targeting Android 35.

| Component                                                   | Recommended install                                                                                                                                                                       |
| ----------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **JDK 17**                                                  | `brew install --cask zulu@17` (macOS) or distro package (Linux). Set `JAVA_HOME` to its install path so `gradlew` uses it explicitly.                                                      |
| **Android command-line tools**                              | Download "Command line tools only" from <https://developer.android.com/studio#command-line-tools-only>. Unzip to `$ANDROID_HOME/cmdline-tools/latest/` (the trailing `latest/` matters).   |
| **Android SDK Platform 35, Build-Tools 35, Platform-Tools** | `sdkmanager "platforms;android-35" "build-tools;35.0.0" "platform-tools"`                                                                                                                  |
| **Android NDK r27c**                                        | `sdkmanager "ndk;27.1.12297006"` (or let `gradlew` download it on first build).                                                                                                            |
| **License acceptance** (one-time)                           | `yes \| sdkmanager --licenses`                                                                                                                                                            |

Then add to your shell rc:

```sh
export ANDROID_HOME="$HOME/Library/Android/sdk"       # macOS default
export PATH="$ANDROID_HOME/cmdline-tools/latest/bin:$PATH"
export PATH="$ANDROID_HOME/platform-tools:$PATH"
export PATH="$ANDROID_HOME/emulator:$PATH"            # optional
export JAVA_HOME="$(brew --prefix zulu@17)"           # macOS via Homebrew
```

(Linux paths: `~/Android/sdk` and a distro-installed JDK 17.)

## Project tasks

Copy `assets/templates/expo-android/tasks.json` from this repo to your Expo project's `.zed/tasks.json`. It defines:

- `android: prebuild` and `android: prebuild (clean)`: generate the native `android/` folder from `app.json`.
- `android: run on device (debug)`: the everyday Fast-Refresh command. Needs Metro running.
- `android: build release APK` and `android: install release APK on connected device`: for the release variant. Release requires a keystore (see below).
- `adb: list devices`, `adb: pair wireless (Android 11+)`: device management.
- `adb: logcat (current app)`: tail logcat filtered to your app's process. Pulls the package name from `app.json`.
- `eas: build preview (cloud)` and `eas: build production (cloud)`: cloud builds via EAS. Requires `npm i -g eas-cli` and `eas login`.

Open the task picker with `cmd-shift-p` then "task: spawn".

## Wireless ADB (Android 11+)

USB tethering is fiddly. Wireless ADB is reliable once paired.

1. On the phone: **Settings > System > Developer options > Wireless debugging > Pair device with pairing code**.
2. In Lathe, run the **adb: pair wireless (Android 11+)** task. Paste the IP:port and pair code from the phone when prompted.
3. After pairing, run `adb connect <ip:port>` (use the **non-pair** port shown in the wireless-debugging screen) once per session. Pairing persists, the connection does not.

## Release keystore (only for local release builds)

Skip this section if you're using EAS preview. EAS handles signing for you.

Generate a release keystore once:

```sh
keytool -genkeypair \
  -alias upload \
  -keyalg RSA -keysize 2048 -validity 36500 \
  -keystore ~/.android/<your-app>-release.keystore \
  -storepass '<strong password>' \
  -dname "CN=Your Name, OU=Dev, O=Lathe, L=City, ST=State, C=US"
```

Add the password to macOS Keychain (don't put it in `gradle.properties` or git):

```sh
security add-generic-password \
  -a "$USER" \
  -s "lathe-android-keystore-<your-app>" \
  -w '<the same password>'
```

Then a launcher script that injects it into gradle (drop into your project under `scripts/build-release.sh`):

```sh
#!/usr/bin/env bash
set -euo pipefail
PWD_VAL=$(security find-generic-password -a "$USER" -s "lathe-android-keystore-<your-app>" -w)
cd android
./gradlew assembleRelease \
  -PMYAPP_RELEASE_STORE_FILE="$HOME/.android/<your-app>-release.keystore" \
  -PMYAPP_RELEASE_KEY_ALIAS=upload \
  -PMYAPP_RELEASE_STORE_PASSWORD="$PWD_VAL" \
  -PMYAPP_RELEASE_KEY_PASSWORD="$PWD_VAL"
```

Your `android/app/build.gradle` should reference those properties in the `signingConfigs { release { ... } }` block. Expo's default template wires this for you when you set `EXPO_RELEASE_KEYSTORE` env, but the manual route above keeps the password out of the env entirely.

## Troubleshooting

| Symptom                                                                  | Cause                                                       | Fix                                                                                            |
| ------------------------------------------------------------------------ | ----------------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| App opens to "could not connect to development server" red screen        | Debug build, Metro not reachable                            | Start `npx expo start` on the laptop, or build a release / EAS preview APK instead.            |
| `Could not determine the dependencies of task ':app:compileDebugKotlin'` | Wrong JDK (often Java 8 from system).                       | `JAVA_HOME=/path/to/jdk-17 ./gradlew ...` or fix shell env.                                    |
| `License for package … not accepted`                                     | First-time SDK install.                                     | `yes \| sdkmanager --licenses`                                                                 |
| `INSTALL_FAILED_INSUFFICIENT_STORAGE`                                    | Phone full.                                                 | Uninstall an old build: `adb uninstall <package>`                                              |
| `INSTALL_FAILED_VERSION_DOWNGRADE`                                       | Device has a higher versionCode than the APK you're pushing | Bump `versionCode` in `app.json`, or `adb uninstall <package>` first.                          |
| Build hangs on first gradle invocation                                   | Downloading NDK (~1 GB) silently                            | Pre-install NDK with `sdkmanager "ndk;27.1.12297006"` so progress is visible.                  |
| `JAVA_HOME points to … which is not Java 17`                             | Multiple JDKs installed                                     | Set `JAVA_HOME` explicitly in `~/.zshrc` per the recipe above.                                 |

## Next phases (planned)

- **Phase 2**: `mobile_dev` crate inside Lathe. Status-bar device picker, dedicated logcat panel, single "Build & Run" command palette action, EAS-build wrapper.
- **Phase 3**: first-run wizard that downloads JDK 17 (Azul Zulu), the Android SDK, and the NDK into a Lathe-managed directory, accepts licenses, and stores release keystore passwords in the macOS Keychain.

Both phases are tracked in the Lathe repo. Phase 1 (this doc) is enough for daily Expo Android dev today.
