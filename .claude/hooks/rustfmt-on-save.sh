#!/bin/sh
# PostToolUse hook: auto-format .rs files after Write/Edit.
# Receives JSON on stdin with tool_input.file_path.

INPUT=$(cat)
FILE=$(echo "$INPUT" | jq -r '.tool_input.file_path // .tool_input.filePath // empty' 2>/dev/null)

# Only act on .rs files.
case "$FILE" in
    *.rs)
        if command -v rustfmt >/dev/null 2>&1; then
            rustfmt "$FILE" 2>/dev/null
        fi
        ;;
esac

exit 0
