# Most of the README's screens, in a wide dark terminal. Run through
# scripts/screenshots, which starts the demo server and sets the environment.
Set Width 1600
Set Height 900
Set Theme "TokyoNight"
Set Shell "bash"
Set FontSize 15
Set Padding 24
Set TypingSpeed 60ms
Hide
Type "export PS1=; clear" Enter
Show
Type "hackney" Enter
Sleep 2.5s
Screenshot front.png

Down
Down
Sleep 1.5s
Screenshot ask.png

Up
Up
Sleep 1s
Type "z"
Sleep 2s
Screenshot zoom.png
Type "z"

Type "?"
Sleep 1.5s
Screenshot help.png
Type "?"

Type "/"
Type "terminal"
Sleep 400ms
Enter
Sleep 2s
Screenshot search.png
Type "q"
