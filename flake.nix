{
  description = "nixly_launcher — daemon + trigger Wayland launcher (OpenGL)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
        };

        # Use the rust-overlay toolchain for the package build too, so dev
        # shell and `nix build`/`nix run` agree on rustc version + Cargo.lock
        # compatibility.
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        nativeBuildInputs = with pkgs; [
          rustToolchain
          pkg-config
          cmake
          wayland-scanner
          makeWrapper
        ];

        buildInputs = with pkgs; [
          wayland
          wayland-protocols
          wlr-protocols
          libxkbcommon

          libGL
          libglvnd
          mesa
          libdrm
          libgbm

          fontconfig
          freetype
          harfbuzz

          dbus
          systemd
        ];

        # Libs the binary opens at runtime: SCTK links wayland-client; glow +
        # khronos-egl `dynamic` feature dlopen libEGL/libGL via libglvnd.
        runtimeLibs = with pkgs; [
          wayland
          libxkbcommon
          libGL
          libglvnd
          mesa
          libdrm
          libgbm
          fontconfig
          freetype
        ];

        # Programs invoked via std::process::Command at runtime.
        runtimePathPkgs = with pkgs; [
          fontconfig    # fc-match for default font lookup
          xdg-utils     # xdg-open for File search / Git projects activation
        ];

        nixly_launcher = rustPlatform.buildRustPackage {
          pname = "nixly_launcher";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          inherit nativeBuildInputs buildInputs;

          # The Wayland scanner needs the protocols XML at build time even
          # though smithay-client-toolkit ships its own copies — keep them
          # exported in case downstream code starts shelling out to scanner.
          WAYLAND_PROTOCOLS_DIR = "${pkgs.wayland-protocols}/share/wayland-protocols";
          WLR_PROTOCOLS_DIR = "${pkgs.wlr-protocols}/share/wlr-protocols";

          postFixup = ''
            for bin in $out/bin/*; do
              wrapProgram "$bin" \
                --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath runtimeLibs}" \
                --prefix PATH : "${pkgs.lib.makeBinPath runtimePathPkgs}"
            done

            # systemd user service — auto-starts the daemon when the user's
            # graphical session comes up. The compositor must opt into systemd
            # integration (Hyprland: systemd.enable = true; sway: systemd-cat)
            # so graphical-session.target is reached.
            mkdir -p $out/share/systemd/user
            cat > $out/share/systemd/user/nixly-launcher.service <<EOF
            [Unit]
            Description=nixly_launcher daemon (Wayland app launcher)
            PartOf=graphical-session.target
            After=graphical-session.target

            [Service]
            Type=simple
            ExecStart=$out/bin/appd
            Restart=on-failure
            RestartSec=3
            StandardOutput=journal
            StandardError=journal

            [Install]
            WantedBy=graphical-session.target
            EOF
          '';

          meta = {
            description = "Daemon + trigger Wayland launcher with layer-shell + OpenGL";
            mainProgram = "appd";
            platforms = pkgs.lib.platforms.linux;
          };
        };
      in
      {
        packages.default = nixly_launcher;
        packages.appd = nixly_launcher;
        packages.apptoggle = nixly_launcher;

        # `nix run`           → daemon (the long-running process)
        # `nix run .#toggle`  → one-shot trigger client
        apps.default = {
          type = "app";
          program = "${nixly_launcher}/bin/appd";
        };
        apps.appd = {
          type = "app";
          program = "${nixly_launcher}/bin/appd";
        };
        apps.toggle = {
          type = "app";
          program = "${nixly_launcher}/bin/apptoggle";
        };

        devShells.default = pkgs.mkShell {
          inherit nativeBuildInputs buildInputs;

          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          RUST_BACKTRACE = "1";

          LD_LIBRARY_PATH = "${pkgs.lib.makeLibraryPath runtimeLibs}";

          WAYLAND_PROTOCOLS_DIR = "${pkgs.wayland-protocols}/share/wayland-protocols";
          WLR_PROTOCOLS_DIR = "${pkgs.wlr-protocols}/share/wlr-protocols";

          shellHook = ''
            echo "nixly_launcher dev shell"
            echo "  rustc:     $(rustc --version)"
            echo "  wayland:   ${pkgs.wayland.version}"
            echo "  protocols: $WAYLAND_PROTOCOLS_DIR"
            echo "  wlr:       $WLR_PROTOCOLS_DIR"
          '';
        };

        formatter = pkgs.nixpkgs-fmt;
      });
}
