#!/usr/bin/env sh
set -eu

# Uninstalls Lathe.

check_remaining_installations() {
    platform="$(uname -s)"
    if [ "$platform" = "Darwin" ]; then
        remaining=$(ls -d /Applications/Lathe*.app 2>/dev/null | wc -l)
        [ "$remaining" -eq 0 ]
    else
        remaining=$(ls -d "$HOME/.local/share/lathe"* 2>/dev/null | wc -l)
        [ "$remaining" -eq 0 ]
    fi
}

prompt_remove_preferences() {
    printf "Do you want to keep your Lathe preferences? [Y/n] "
    read -r response
    case "$response" in
        [nN]|[nN][oO])
            rm -rf "$HOME/.config/zed"
            echo "Preferences removed."
            ;;
        *)
            echo "Preferences kept."
            ;;
    esac
}

main() {
    platform="$(uname -s)"
    channel="${ZED_CHANNEL:-stable}"

    if [ "$platform" = "Darwin" ]; then
        platform="macos"
    elif [ "$platform" = "Linux" ]; then
        platform="linux"
    else
        echo "Unsupported platform $platform"
        exit 1
    fi

    "$platform"

    echo "Lathe has been uninstalled"
}

linux() {
    suffix=""
    if [ "$channel" != "stable" ]; then
        suffix="-$channel"
    fi

    app_id=""
    db_suffix="stable"
    case "$channel" in
      stable)
        app_id="dev.lathe.lathe"
        db_suffix="stable"
        ;;
      nightly)
        app_id="dev.lathe.lathe-Nightly"
        db_suffix="nightly"
        ;;
      preview)
        app_id="dev.lathe.lathe-Preview"
        db_suffix="preview"
        ;;
      beta)
        app_id="dev.lathe.lathe-Beta"
        db_suffix="beta"
        ;;
      dev)
        app_id="dev.lathe.lathe-Dev"
        db_suffix="dev"
        ;;
      *)
        echo "Unknown release channel: ${channel}. Using stable app ID."
        app_id="dev.lathe.lathe"
        db_suffix="stable"
        ;;
    esac

    rm -rf "$HOME/.local/share/lathe${suffix}"
    rm -f "$HOME/.local/bin/lathe"
    rm -f "$HOME/.local/share/applications/${app_id}.desktop"
    rm -f "$HOME/.local/share/icons/hicolor/512x512/apps/lathe.png"
    rm -f "$HOME/.local/share/icons/hicolor/1024x1024/apps/lathe.png"

    # Lathe still writes its internal data to Zed-named paths.
    rm -rf "$HOME/.local/share/zed/db/0-$db_suffix"
    rm -f "$HOME/.local/share/zed/zed-$db_suffix.sock"

    if check_remaining_installations; then
        rm -rf "$HOME/.local/share/zed"
        prompt_remove_preferences
    fi

    rm -rf "$HOME/.zed_server"
}

macos() {
    app="Lathe.app"
    db_suffix="stable"
    app_id="dev.lathe.lathe"
    case "$channel" in
      nightly)
        app="Lathe Nightly.app"
        db_suffix="nightly"
        app_id="dev.lathe.lathe-Nightly"
        ;;
      preview)
        app="Lathe Preview.app"
        db_suffix="preview"
        app_id="dev.lathe.lathe-Preview"
        ;;
      beta)
        app="Lathe Beta.app"
        db_suffix="beta"
        app_id="dev.lathe.lathe-Beta"
        ;;
      dev)
        app="Lathe Dev.app"
        db_suffix="dev"
        app_id="dev.lathe.lathe-Dev"
        ;;
    esac

    if [ -d "/Applications/$app" ]; then
        rm -rf "/Applications/$app"
    fi

    rm -f "/usr/local/bin/lathe" 2>/dev/null \
        || sudo rm -f "/usr/local/bin/lathe"

    # Lathe still writes its internal data to Zed-named paths.
    rm -rf "$HOME/Library/Application Support/Zed/db/0-$db_suffix"

    rm -rf "$HOME/Library/Application Support/com.apple.sharedfilelist/com.apple.LSSharedFileList.ApplicationRecentDocuments/$app_id.sfl"*
    rm -rf "$HOME/Library/Caches/$app_id"
    rm -rf "$HOME/Library/HTTPStorages/$app_id"
    rm -rf "$HOME/Library/Preferences/$app_id.plist"
    rm -rf "$HOME/Library/Saved Application State/$app_id.savedState"

    if check_remaining_installations; then
        rm -rf "$HOME/Library/Application Support/Zed"
        rm -rf "$HOME/Library/Logs/Zed"

        prompt_remove_preferences
    fi

    rm -rf "$HOME/.zed_server"
}

main "$@"
