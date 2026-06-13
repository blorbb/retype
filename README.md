# retype

a global keyboard remapper

it works on my machine™️

## features

caps-lock "layer"

- caps+{i,j,k,l} -> up, left, down, right
- caps+{h,;} -> home, end
- press super+caps without anything else to toggle caps lock
- caps+numbers then any key -> repeats that key with the number you typed (up to 65535)
- caps+{d,f} then any character -> searches backwards/forwards from your cursor and goes to that character
  - if the cursor is already next to that character, it searches for the next one. e.g. when the cursor is at `|'something'`, pressing caps+f then `'` moves the cursor to `'something'|`
- caps+shift+{d,f} then any character -> searches and selects backwards/forwards from your cursor
- kill retype with caps+f1

## usage

only works on Linux, using evdev.

run `sudo usermod -aG tty,input "$USER"` to add yourself to the `tty` and `input` groups.

install via `cargo install --path .`. to start from a terminal, run `retype & disown`.
