{
  description = "Volumes: a COSMIC panel applet for drives, phones and network shares";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      lib = nixpkgs.lib;

      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      cargoToml = (fromTOML (builtins.readFile ./cosmic-ext-applet-volumes/Cargo.toml)).package;

      # Packaged the way nixpkgs packages cosmic-applets: libcosmicAppHook takes care of the
      # libraries libcosmic/iced load at run time (wayland, xkbcommon, ...) and of the icon and
      # theme search paths, and the justfile does the installing.
      package =
        pkgs:
        pkgs.callPackage (
          {
            lib,
            stdenv,
            rustPlatform,
            just,
            pkg-config,
            util-linuxMinimal, # `rev`, used by the justfile's icon install recipe
            libcosmicAppHook,
            glib, # GIO, for phones (MTP) through GVfs
          }:
          rustPlatform.buildRustPackage {
            pname = cargoToml.name;
            version = cargoToml.version;

            src = lib.fileset.toSource {
              root = ./.;
              fileset = lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./clippy.toml
                ./justfile
                ./cosmic-ext-applet-volumes
              ];
            };

            # Fetches the git dependencies (libcosmic and friends) at their locked revisions,
            # so there is no cargoHash to keep up to date. Fine for a flake of your own; it is
            # not accepted for packages inside nixpkgs.
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };

            nativeBuildInputs = [
              just
              pkg-config
              util-linuxMinimal
              libcosmicAppHook
            ];

            buildInputs = [ glib ];

            dontUseJustBuild = true;
            dontUseJustCheck = true;

            justFlags = [
              "--set"
              "prefix"
              (placeholder "out")
              "--set"
              "target"
              "${stdenv.hostPlatform.rust.cargoShortTarget}/release"
            ];

            # Tests run as `nix flake check` (see `checks` below) rather than on every build.
            doCheck = false;

            meta = {
              description = "COSMIC panel applet to open, mount and safely remove drives, phones and network shares";
              longDescription = ''
                Unmaintained: no maintainer, no support, no warranty. Use at your own risk.
              '';
              license = lib.licenses.gpl3Only;
              mainProgram = cargoToml.name;
              platforms = lib.platforms.linux;
            };
          }
        ) { };
    in
    {
      packages = forAllSystems (pkgs: rec {
        cosmic-ext-applet-volumes = package pkgs;
        default = cosmic-ext-applet-volumes;
      });

      # Adds `pkgs.cosmic-ext-applet-volumes`, e.g. for
      #   environment.systemPackages = [ pkgs.cosmic-ext-applet-volumes ];
      overlays.default = final: _prev: {
        cosmic-ext-applet-volumes = package final;
      };

      # NixOS module: installs the applet and turns on the two services it relies on: UDisks2
      # (disks) and GVfs (phones over MTP and Samba/NFS shares: its nixpkgs package is built with
      # libmtp, samba and libnfs, and the module brings the libmtp udev rules, FUSE and the GIO
      # modules). services.desktopManager.cosmic already enables both as a side effect of
      # enabling gvfs; this makes the dependency explicit.
      #   imports = [ inputs.volumes.nixosModules.default ];
      #   programs.cosmic-ext-applet-volumes.enable = true;
      nixosModules.default =
        { config, lib, pkgs, ... }:
        {
          options.programs.cosmic-ext-applet-volumes.enable =
            lib.mkEnableOption "the Volumes COSMIC panel applet, and the UDisks2 and GVfs services it talks to (disks; phones and network shares)";

          config = lib.mkIf config.programs.cosmic-ext-applet-volumes.enable {
            environment.systemPackages = [ (package pkgs) ];
            services.udisks2.enable = lib.mkDefault true;
            services.gvfs.enable = lib.mkDefault true;
          };
        };

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          # libcosmicAppHook also sets the linker flags for the dlopen'd libraries in the shell.
          nativeBuildInputs = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            just
            pkg-config
            util-linuxMinimal
            libcosmicAppHook
            dbus # dbus-daemon, for tests/e2e.rs
          ];
          buildInputs = [ pkgs.glib ];
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      });

      # Unit tests plus the end-to-end test against a fake UDisks2 on a private D-Bus.
      checks = forAllSystems (pkgs: {
        tests = (package pkgs).overrideAttrs (old: {
          doCheck = true;
          nativeCheckInputs = (old.nativeCheckInputs or [ ]) ++ [ pkgs.dbus ];
        });
      });
    };
}
