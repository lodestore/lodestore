Output .out/install-first-backup.gif

Require orb

Set Shell "bash"
Set FontFamily "Menlo"
Set FontSize 14
Set Width 1200
Set Height 720
Set Padding 12
Set LineHeight 1.25
Set TypingSpeed 30ms
Set Framerate 15
Set CursorBlink false
Set Theme { "name": "ink", "black": "#070a12", "red": "#ff7b72", "green": "#7ee787", "yellow": "#e3b341", "blue": "#79c0ff", "magenta": "#d2a8ff", "cyan": "#56d4dd", "white": "#c9d1d9", "brightBlack": "#6e7681", "brightRed": "#ffa198", "brightGreen": "#56d364", "brightYellow": "#f2cc60", "brightBlue": "#a5d6ff", "brightMagenta": "#e2c5ff", "brightCyan": "#b3f0ff", "brightWhite": "#f0f6fc", "background": "#070a12", "foreground": "#e6edf3", "selection": "#1f2a44", "cursor": "#e6edf3" }

Hide
Type "orb -m lodestore-test" Enter
Sleep 3s
Type "cd && unset PROMPT_COMMAND && export PS1='$ ' && . ~/.vhs/session.sh && clear" Enter
Sleep 1s
Show

Type "export LODESTORE_HOST=$(hostname -I | cut -d' ' -f1)" Enter
Sleep 800ms
Type "export LODESTORE_VERSION=0.1.0" Enter
Sleep 600ms
Type "docker run -d --name lodestore \" Enter
Type "  -p 8080:8080 -p 8443:8443 \" Enter
Type "  -v lodestore-data:/var/lib/lodestore \" Enter
Type `  -e LODESTORE_PUBLIC_URL="http://$LODESTORE_HOST:8080" \` Enter
Type `  -e LODESTORE_GRPC_PUBLIC_ADDRESS="$LODESTORE_HOST:8443" \` Enter
Type "  ghcr.io/lodestore/lodestore:$LODESTORE_VERSION" Enter
Wait+Line@60s /^\$$/
Sleep 3s
Type "docker logs lodestore 2>&1 | grep 'setup?token='" Enter
Sleep 2500ms
Type "# set up the administrator and the repository in the web interface, then add a host" Enter
Sleep 2s

Hide
Type "vhs_setup && clear" Enter
Wait+Line@120s /^\$$/
Show

Type "# on the host to back up, with the join token that Add host shows" Enter
Sleep 1s
Type `curl -fsSL "http://$LODESTORE_HOST:8080/install.sh" | sudo sh -s -- --insecure --join "$JOIN_TOKEN"` Enter
Wait+Line@90s /^\$$/
Sleep 1500ms

Type "lode server policies run documents" Enter
Sleep 1500ms
Type "lode server jobs logs $JOB --follow" Enter
Wait+Line@120s /^\$$/
Sleep 1500ms

Type "lode server snapshots list --host lodestore-test" Enter
Wait+Line@30s /^\$$/
Sleep 4s
