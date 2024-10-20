# retype

a global keyboard remapper

it works on my machine™️

## features

caps-lock "layer"
- caps+{i,j,k,l} -> up, left, down, right
- caps+{h,;} -> home, end
- pressing caps does not toggle caps-lock: press super+caps instead
- caps+numbers then any key -> repeats that key with the number you typed (up to 65535)
- caps+{d,f} then any character -> searches backwards/forwards from your cursor and goes to that character
    - if the cursor is already next to that character, it searches for the next one. e.g. when the cursor is at |'something', pressing caps+f then ' moves the cursor to 'something'|
- caps+shift+{d,f} then any character -> searches and selects backwards/forwards from your cursor
- toggle all hotkeys with ctrl+super+k
- logs to data dir/retype.log

## "features"

- forks of [`rdev`](https://github.com/Narsil/rdev) and [`selection`](https://github.com/pot-app/Selection) for minor fixes with stuff i need
- zero unsafe (in the part that i actually made) (rdev probably has undefined behaviour) (i tried to fix some of it)
- zero configuration, everything is hard coded, including icons (icon names must exist on your system)
- no testing on windows or mac, but it maybe works (crates used are cross platform)
- program quits by panicking (idk how else to stop the rdev hook)
- occasionally blocks all input for a short period of time when new devices are connected (idk why)
- no documentation (the design is very human)
- built in rust 🚀🚀 blazingly fast 🚀🚀

## usage

For linux:

run `sudo usermod -aG tty,input "$USER"` to add yourself to the `tty` and `input` groups.

run `cargo build --release` to make the executable. Start from the terminal, run `./target/release/retype & disown`.

logs are stored in `~/.local/share/retype.log` or wherever the data directory is for your OS.

## contributing

it works for me, it (probably) wont work for you

if you want to use this for some reason, feel free to make a pr with fixes

maybe someday ill make this thing actually configurable
