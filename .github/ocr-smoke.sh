#!/bin/bash
# OCR and clipboard smoke test of a built screenrec, the same on the CI
# runners and on a developer's machine. It puts known text on screen and
# replaces the clipboard: on Linux run it inside a private X server
# (xvfb-run -a -s '-screen 0 1280x720x24' ...), never on your own desktop.
# Needs ffmpeg, ffplay and xclip on Linux; PowerShell on Windows.
#   shot --ocr         -> <shot>.txt holds the text on screen (accents, Japanese)
#   shot --ocr --clip  -> that text is on the clipboard after screenrec exits; no .txt
#   shot --clip        -> an image is on the clipboard after screenrec exits
#   3 shots at once    -> 3 .txt files: one shared service serves them all
#   rec --ocr (Linux)  -> <video>.txt lines "HH:MM:SS.mmm (HH:MM:SS) - text"
# usage: ocr-smoke.sh <screenrec> [out dir]
set -u
BIN=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
OUT=${2:-ocr-smoke}
mkdir -p "$OUT" && OUT=$(cd "$OUT" && pwd)
LATIN='Hola señor, ¿qué tal?'
JA='日本語のテキスト'
fails=0
ok() { echo "PASS $*"; }
ko() { echo "FAIL $*"; fails=$((fails + 1)); }
# poll <seconds> <command...>: true as soon as the command is
poll() {
    local n=$1
    shift
    for _ in $(seq "$n"); do "$@" && return 0; sleep 1; done
    return 1
}
read_back() { [ -s "$1" ] && grep -q Hola "$1"; }

case "$(uname -s)" in
Linux)
    clip_set() { printf %s "$1" | timeout 5 xclip -selection clipboard; }
    clip_text() { timeout 5 xclip -o -selection clipboard 2>/dev/null; }
    clip_image() { timeout 5 xclip -o -selection clipboard -t TARGETS 2>/dev/null | grep -qx image/png; }
    show() {
        printf %s "$LATIN" > "$OUT/latin.txt"
        printf %s "$JA" > "$OUT/ja.txt"
        local vf="drawtext=fontfile=$(fc-match -f '%{file}' 'DejaVu Sans'):textfile=$OUT/latin.txt:fontsize=64:x=40:y=80"
        JA_FONT=$(fc-list :lang=ja file | head -1 | cut -d: -f1)
        [ -n "$JA_FONT" ] && vf="$vf,drawtext=fontfile=$JA_FONT:textfile=$OUT/ja.txt:fontsize=64:x=40:y=220"
        ffmpeg -v error -y -f lavfi -i color=white:s=1280x720 -frames:v 1 -vf "$vf" "$OUT/text.png" || ko "ffmpeg drawtext"
        timeout 600 ffplay -loglevel quiet -noborder -left 0 -top 0 "$OUT/text.png" &
        SHOWN=$!
    }
    ;;
MINGW* | MSYS* | CYGWIN*)
    # -EncodedCommand: UTF-16 all the way, so ñ and ¿ reach PowerShell intact.
    pwsh_run() { powershell -NoProfile -EncodedCommand "$(printf %s "$1" | iconv -f UTF-8 -t UTF-16LE | base64 -w0)" | tr -d '\r'; }
    clip_set() { pwsh_run "Set-Clipboard -Value '$1'"; }
    clip_text() { pwsh_run 'Get-Clipboard -Raw'; }
    clip_image() { [ "$(pwsh_run 'Add-Type -AssemblyName System.Windows.Forms; [Windows.Forms.Clipboard]::ContainsImage()')" = True ]; }
    show() {
        JA_FONT=1 # Yu Gothic / Meiryo ship with Windows
        pwsh_run "Add-Type -AssemblyName System.Windows.Forms
            \$f = New-Object Windows.Forms.Form -Property @{ TopMost = \$true; WindowState = 'Maximized'; BackColor = 'White'; FormBorderStyle = 'None' }
            \$l = New-Object Windows.Forms.Label -Property @{ Text = \"$LATIN\`n$JA\"; AutoSize = \$true; ForeColor = 'Black'; Location = '40,80' }
            \$l.Font = New-Object Drawing.Font('Yu Gothic UI', 48)
            \$f.Controls.Add(\$l)
            [Windows.Forms.Application]::Run(\$f)" &
        SHOWN=$!
    }
    ;;
Darwin)
    clip_set() { printf %s "$1" | pbcopy; }
    clip_text() { pbpaste; }
    clip_image() { osascript -e 'clipboard info' | grep -q PNGf; }
    show() {
        JA_FONT=1
        osascript -e "display dialog \"$LATIN\" & return & \"$JA\" giving up after 600" > /dev/null &
        SHOWN=$!
    }
    ;;
*)
    echo "unknown OS $(uname -s)"
    exit 2
    ;;
esac

show
sleep 3

# 1. shot --ocr: the text, in a .txt next to the image (the OCR finishes after screenrec exits)
"$BIN" shot "$OUT/a.png" --ocr || ko "shot --ocr exited with $?"
if poll 300 read_back "$OUT/a.txt"; then ok "shot --ocr: $(tr '\n' ' ' < "$OUT/a.txt")"; else ko "shot --ocr: no 'Hola' in a.txt: $(cat "$OUT/a.txt" 2> /dev/null)"; fi
grep -q 'señor' "$OUT/a.txt" 2> /dev/null && ok "accents read (señor)" || ko "accents: no 'señor'"
if [ -n "${JA_FONT:-}" ]; then grep -q '日本語' "$OUT/a.txt" 2> /dev/null && ok "Japanese read" || ko "Japanese: no '日本語'"; fi

# 2. shot --ocr --clip: the text on the clipboard, still there after screenrec exited; no .txt
clip_set reset
"$BIN" shot "$OUT/b.png" --ocr --clip || ko "shot --ocr --clip exited with $?"
says_hola() { clip_text | grep -q Hola; }
if poll 120 says_hola; then ok "shot --ocr --clip: the text is on the clipboard"; else ko "shot --ocr --clip: clipboard has '$(clip_text | head -c 100)'"; fi
sleep 2
[ ! -e "$OUT/b.txt" ] && ok "no b.txt with --clip" || ko "b.txt written although --clip"

# 3. shot --clip: the image on the clipboard after screenrec exited
clip_set reset
"$BIN" shot "$OUT/c.png" --clip || ko "shot --clip exited with $?"
[ -s "$OUT/c.png" ] && clip_image && ok "shot --clip: an image on the clipboard, and c.png" || ko "shot --clip: no image on the clipboard"

# 4. three requests at once, from three processes: the one service reads them all
pids=
for n in 1 2 3; do
    "$BIN" shot "$OUT/f$n.png" --ocr &
    pids="$pids $!"
done
wait $pids
for n in 1 2 3; do poll 120 read_back "$OUT/f$n.txt" && ok "concurrent request $n" || ko "concurrent request $n: no text"; done

# 5. rec --ocr: one line per detection, video time then wall-clock time
if [ "$(uname -s)" = Linux ]; then
    "$BIN" rec "$OUT/r.mkv" --ocr --cpu &
    REC=$!
    sleep 8
    kill -INT $REC
    wait $REC || ko "rec --ocr exited with $?"
    line='^[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3} \([0-9]{2}:[0-9]{2}:[0-9]{2}\) - '
    rec_read() { [ -s "$OUT/r.txt" ] && grep -qE "${line}.*Hola" "$OUT/r.txt"; }
    if poll 120 rec_read; then ok "rec --ocr: $(head -3 "$OUT/r.txt" | tr '\n' '|')"; else ko "rec --ocr: r.txt is '$(head -3 "$OUT/r.txt" 2> /dev/null)'"; fi
    bad=$(grep -cvE "$line" "$OUT/r.txt" 2> /dev/null)
    [ "${bad:-1}" = 0 ] && ok "every r.txt line has the format" || ko "$bad lines of r.txt without the format"
fi

kill "$SHOWN" 2> /dev/null
echo "$fails failed"
[ "$fails" = 0 ]
