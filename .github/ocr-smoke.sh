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
# On Linux the service is started first as one left over from an earlier X server on this
# display (a restart, a re-login, the last xvfb-run): with an XAUTHORITY that is gone. All of
# the above goes through it, so what it copies must still land where its client is.
# Each shot waits until screenshots show the text drawn and still; the files stay in the out dir.
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
# drawn <image> [moving]: true once two screenshots in a row (the last is $OUT/now.png) differ
# from <image> and agree, or with "moving" (a clock) differ from each other too: what was
# opened is drawn and still, or drawn and running, not a window still starting. 60 s at most.
drawn() {
    local end=$((SECONDS + 60))
    rm -f "$OUT/prev.png"
    while [ $SECONDS -lt $end ]; do
        "$BIN" shot "$OUT/now.png" > /dev/null || return 1
        if [ -e "$OUT/prev.png" ] && ! cmp -s "$OUT/now.png" "$1" && ! cmp -s "$OUT/prev.png" "$1"; then
            if cmp -s "$OUT/now.png" "$OUT/prev.png"; then [ -z "${2:-}" ] && return 0; else [ -n "${2:-}" ] && return 0; fi
        fi
        cp "$OUT/now.png" "$OUT/prev.png"
        sleep 0.5
    done
    return 1
}
# what a failed read had to go on: the screenshot, and the .txt the service wrote
seen() {
    if cmp -s "$1" "$OUT/text-shown.png"; then echo "$(basename "$1") shows the text"
    elif cmp -s "$1" "$OUT/before.png"; then echo "$(basename "$1") is the screen before the text (blank on Xvfb): the text wasn't drawn yet"
    else echo "$(basename "$1") is neither the screen with the text nor the one before it"; fi
}
wrote() {
    if [ ! -e "$1" ]; then echo "no $(basename "$1") (the service never wrote it)"
    elif [ ! -s "$1" ]; then echo "$(basename "$1") is empty (the service read no text)"
    else echo "$(basename "$1") has '$(tr '\n' ' ' < "$1")'"; fi
}

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
        # Japanese only where the font is there (a Windows Server runner may lack it)
        JA_FONT=$(pwsh_run "Add-Type -AssemblyName System.Drawing; [Drawing.FontFamily]::Families.Name -contains 'Yu Gothic UI'")
        [ "$JA_FONT" = True ] || JA_FONT=
        # Back once the form says it is up and painted: the taskbar clock may change the screen first.
        pwsh_run "Add-Type -AssemblyName System.Windows.Forms
            \$f = New-Object Windows.Forms.Form -Property @{ TopMost = \$true; WindowState = 'Maximized'; BackColor = 'White'; FormBorderStyle = 'None' }
            \$l = New-Object Windows.Forms.Label -Property @{ Text = \"$LATIN\`n$JA\"; AutoSize = \$true; ForeColor = 'Black'; Location = '40,80' }
            \$l.Font = New-Object Drawing.Font('Yu Gothic UI', 48)
            \$f.Controls.Add(\$l)
            \$f.Add_Shown({ \$f.Refresh(); New-Item -ItemType File -Force '$(cygpath -w "$OUT/up")' | Out-Null })
            [Windows.Forms.Application]::Run(\$f)" &
        SHOWN=$!
        poll 60 test -e "$OUT/up"
    }
    ;;
Darwin)
    clip_set() { printf %s "$1" | pbcopy; }
    clip_text() { pbpaste; }
    clip_image() { osascript -e 'clipboard info' | grep -q PNGf; }
    show() {
        JA_FONT=1
        # Back once the dialog is about to open: the menu bar clock may change the screen first.
        osascript -e "do shell script \"touch '$OUT/up'\"" -e "display dialog \"$LATIN\" & return & \"$JA\" giving up after 600" > /dev/null &
        SHOWN=$!
        poll 60 test -e "$OUT/up"
    }
    ;;
*)
    echo "unknown OS $(uname -s)"
    exit 2
    ;;
esac

if [ "$(uname -s)" = Linux ]; then
    XAUTHORITY=$OUT/gone "$BIN" ocrd & # the service, as left over from a gone X server (see the top)
    STALE=$!
fi
# what an earlier run left here would pass for this run's answers (r.txt is appended to)
rm -f "$OUT"/{a,b,c,f1,f2,f3}.{png,txt} "$OUT"/r.{mkv,txt} "$OUT"/{up,before.png,now.png,prev.png,text-shown.png}
"$BIN" shot "$OUT/before.png" > /dev/null || ko "shot of the screen before the text exited with $?"
start=$SECONDS
show
if drawn "$OUT/before.png"; then
    cp "$OUT/now.png" "$OUT/text-shown.png"
    echo "the text is on screen, $((SECONDS - start)) s after it was opened"
else
    ko "the text never showed on screen in 60 s (the last screenshot is now.png)"
fi

# 1. shot --ocr: the text, in a .txt next to the image (the OCR finishes after screenrec exits)
"$BIN" shot "$OUT/a.png" --ocr || ko "shot --ocr exited with $?"
if poll 300 read_back "$OUT/a.txt"; then ok "shot --ocr: $(tr '\n' ' ' < "$OUT/a.txt")"; else ko "shot --ocr: no 'Hola': $(wrote "$OUT/a.txt"); $(seen "$OUT/a.png")"; fi
grep -q 'señor' "$OUT/a.txt" 2> /dev/null && ok "accents read (señor)" || ko "accents: no 'señor'"
if [ -n "${JA_FONT:-}" ]; then grep -q '日本語' "$OUT/a.txt" 2> /dev/null && ok "Japanese read" || ko "Japanese: no '日本語'"; fi

# 2. shot --ocr --clip: the text on the clipboard, still there after screenrec exited; no .txt
clip_set reset
"$BIN" shot "$OUT/b.png" --ocr --clip || ko "shot --ocr --clip exited with $?"
says_hola() { clip_text | grep -q Hola; }
by=
if [ -n "${STALE:-}" ]; then
    by=" (by the service started for a gone X server)"
    kill -0 "$STALE" 2> /dev/null || by=" (by a service that was already running, not the one started for a gone X server)"
fi
if poll 120 says_hola; then ok "shot --ocr --clip: the text is on the clipboard$by"; else ko "shot --ocr --clip: clipboard has '$(clip_text | head -c 100)'; $(seen "$OUT/b.png")$by"; fi
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
for n in 1 2 3; do poll 120 read_back "$OUT/f$n.txt" && ok "concurrent request $n" || ko "concurrent request $n: $(wrote "$OUT/f$n.txt"); $(seen "$OUT/f$n.png")"; done

# 5. rec --ocr: one line per detection, video time then wall-clock time. A running clock on
# screen gives every sample new text, so the reads every 2 s (the default) each make a line.
if [ "$(uname -s)" = Linux ]; then
    timeout 60 ffplay -loglevel quiet -noborder -left 0 -top 0 -f lavfi -i "color=white:s=1280x720:r=10,drawtext=fontfile=$(fc-match -f '%{file}' 'DejaVu Sans'):text='Hola %{pts\\:hms}':fontsize=64:x=40:y=360" &
    CLOCK=$!
    drawn "$OUT/text-shown.png" moving || ko "the clock never showed on screen in 60 s"
    "$BIN" rec "$OUT/r.mkv" --ocr --cpu &
    REC=$!
    sleep 9
    kill -INT $REC
    wait $REC || ko "rec --ocr exited with $?"
    kill $CLOCK 2> /dev/null
    line='^[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3} \([0-9]{2}:[0-9]{2}:[0-9]{2}\) - '
    rec_read() { [ -s "$OUT/r.txt" ] && [ "$(grep -cE "${line}Hola" "$OUT/r.txt")" -ge 3 ]; }
    if poll 120 rec_read; then ok "rec --ocr: $(grep -cE "${line}Hola" "$OUT/r.txt") reads: $(head -4 "$OUT/r.txt" | tr '\n' '|')"; else ko "rec --ocr: r.txt is '$(head -5 "$OUT/r.txt" 2> /dev/null | tr '\n' '|')'"; fi
    bad=$(grep -cvE "$line" "$OUT/r.txt" 2> /dev/null)
    [ "${bad:-1}" = 0 ] && ok "every r.txt line has the format" || ko "$bad lines of r.txt without the format"
    # the video times climb, about 2 s apart (0.5 s of slack for a busy machine)
    gaps=$(grep -oE '^[0-9:.]+' "$OUT/r.txt" | awk -F: '{t = $1 * 3600 + $2 * 60 + $3; if (NR > 1) printf "%.1f ", t - p; p = t}')
    echo "$gaps" | awk '{for (i = 1; i <= NF; i++) if ($i < 1.5) bad = 1} END {exit bad}' && ok "reads every ~2 s: gaps $gaps" || ko "read gaps: $gaps"
fi

kill "$SHOWN" ${STALE:-} 2> /dev/null
echo "$fails failed"
[ "$fails" = 0 ]
