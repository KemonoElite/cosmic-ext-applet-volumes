name := 'cosmic-ext-applet-volumes'
appid := 'local.cosmicext.Volumes'

rootdir := ''
prefix := '/usr'
debug := '0'
vendor := '0'
target := if debug == '1' { 'debug' } else { 'release' }
vendor_args := if vendor == '1' { '--frozen --offline' } else { '' }
debug_args := if debug == '1' { '' } else { '--release' }
cargo_args := vendor_args + ' ' + debug_args

targetdir := env('CARGO_TARGET_DIR', 'target')
sharedir := rootdir + prefix + '/share'
iconsdir := sharedir + '/icons/hicolor'
bindir := rootdir + prefix + '/bin'

default: build-release

# Compiles with debug profile
build-debug *args:
    cargo build {{ args }}

# Compiles with release profile
build-release *args: (build-debug '--release' args)

# Compile with a vendored tarball
build-vendored *args: vendor-extract (build-release '--frozen --offline' args)

# Check for errors and linter warnings
check *args:
    cargo clippy --all-features {{ args }} -- -W clippy::pedantic

# Runs a check with JSON message format for IDE integration
check-json: (check '--message-format=json')

# Run the unit tests
test *args:
    cargo test {{ args }}

# Run the applet outside the panel (needs a Wayland session)
run *args:
    env RUST_BACKTRACE=full cargo run --release {{ args }}

# Remove Cargo build artifacts
clean:
    cargo clean

# Also remove .cargo and vendored dependencies
clean-dist: clean
    rm -rf .cargo vendor vendor.tar target

_install_icons:
    find {{ name }}/'data'/'icons' -type f -exec echo {} \; | rev | cut -d'/' -f-3 | rev | xargs -d '\n' -I {} install -Dm0644 {{ name }}/'data'/'icons'/{} {{ iconsdir }}/{}

# Installs files into the system
install: _install_icons
    install -Dm0755 {{ targetdir }}/{{ target }}/{{ name }} {{ bindir }}/{{ name }}
    install -Dm0644 {{ name }}/data/{{ appid }}.desktop {{ sharedir }}/applications/{{ appid }}.desktop

# Uninstalls installed files
uninstall:
    rm -f {{ bindir }}/{{ name }} {{ sharedir }}/applications/{{ appid }}.desktop {{ iconsdir }}/scalable/apps/{{ appid }}-symbolic.svg

# Vendor Cargo dependencies locally
vendor:
    #!/usr/bin/env bash
    mkdir -p .cargo
    cargo vendor --locked | head -n -1 > .cargo/config.toml
    echo 'directory = "vendor"' >> .cargo/config.toml
    tar pcf vendor.tar .cargo vendor
    rm -rf .cargo vendor

# Extracts vendored dependencies
[private]
vendor-extract:
    rm -rf vendor
    tar pxf vendor.tar
