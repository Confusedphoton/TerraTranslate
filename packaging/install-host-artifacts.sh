#!/bin/sh
set -eu

target_dir=${1:-target}
destdir=${DESTDIR:-}
prefix=${PREFIX:-/usr}

install -Dm755 "$target_dir/release/terratranslate" "$destdir$prefix/bin/terratranslate"
install -Dm755 "$target_dir/release/libterratranslate_native_hook.so" \
    "$destdir$prefix/lib/terratranslate/libterratranslate_native_hook.so"
install -Dm755 "$target_dir/i686-pc-windows-gnu/release/terratranslate-wine-injector.exe" \
    "$destdir$prefix/libexec/terratranslate/wine/i686/terratranslate-wine-injector.exe"
install -Dm755 "$target_dir/i686-pc-windows-gnu/release/terratranslate_wine_hook.dll" \
    "$destdir$prefix/lib/terratranslate/wine/i686/terratranslate_wine_hook.dll"
install -Dm755 "$target_dir/x86_64-pc-windows-gnu/release/terratranslate-wine-injector.exe" \
    "$destdir$prefix/libexec/terratranslate/wine/x86_64/terratranslate-wine-injector.exe"
install -Dm755 "$target_dir/x86_64-pc-windows-gnu/release/terratranslate_wine_hook.dll" \
    "$destdir$prefix/lib/terratranslate/wine/x86_64/terratranslate_wine_hook.dll"
install -Dm644 crates/terratranslate-wine-hook/THIRD_PARTY_LICENSES/MinHook.txt \
    "$destdir$prefix/share/licenses/terratranslate/MinHook.txt"
install -Dm644 vendor/minhook/LICENSE \
    "$destdir$prefix/share/licenses/terratranslate/minhook-rust-wrapper.txt"
