{
  description = "pty - persistent terminal sessions with detach/attach, in Rust on libghostty";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
        inherit (pkgs) lib;

        # Cargo.toml is the single source of truth for the version; a release
        # bump needs no matching edit here.
        version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;

        # `crates/pty/build.rs` stamps `<version>+<PTY_BUILD_SHA>` into
        # `pty version`. A flake source carries no `.git`, so the commit comes
        # from the flake's own metadata. A dirty tree reports its base commit,
        # the same way `git rev-parse --short HEAD` does for a local build.
        buildSha = self.shortRev or (lib.removeSuffix "-dirty" (self.dirtyShortRev or "dirty"));

        # The Ghostty commit that libghostty-vt-sys 0.2.1 pins (its build.rs:7).
        # That build script clones it at build time unless GHOSTTY_SOURCE_DIR
        # names a checkout, and lets `zig build` fetch Ghostty's own packages
        # unless GHOSTTY_ZIG_SYSTEM_DIR names a pre-populated package directory.
        # The build sandbox has no network, so both are fixed-output fetches.
        ghosttyRev = "a887df42c56f6de86c0fe6da9c4eeca37931e083";
        ghosttyShortRev = lib.substring 0 7 ghosttyRev;

        ghosttySrc = pkgs.fetchgit {
          name = "ghostty-${ghosttyShortRev}-src";
          url = "https://github.com/ghostty-org/ghostty.git";
          rev = ghosttyRev;
          hash = "sha256-1Zz65SCk3rkJ9+Q0MmyNOTNiDSLBRIHRd3IvFM4iNXw=";
        };

        # Ghostty's zig package cache, in the layout `zig build --system <dir>`
        # reads (one directory per package, named by its content hash). Fetched
        # with Ghostty's own script because `zig build --fetch` skips transitive
        # dependencies (ziglang/zig#20976); the script walks build.zig.zon.txt,
        # the full transitive list Ghostty checks in. The hash is what this
        # recipe produced on 2026-08-29; it is not Ghostty's nix/zigCacheHash.nix,
        # whose flake fetches with a different recipe.
        ghosttyZigDeps = pkgs.stdenvNoCC.mkDerivation {
          name = "ghostty-${ghosttyShortRev}-zig-deps";
          src = ghosttySrc;

          nativeBuildInputs = [
            pkgs.cacert
            pkgs.git
            pkgs.zig_0_15
          ];

          dontConfigure = true;
          dontFixup = true;

          buildPhase = ''
            runHook preBuild
            export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-global-cache"
            ./nix/build-support/fetch-zig-cache.sh
            runHook postBuild
          '';

          installPhase = ''
            runHook preInstall
            mv "$ZIG_GLOBAL_CACHE_DIR/p" "$out"
            runHook postInstall
          '';

          outputHashMode = "recursive";
          outputHash = "sha256-PnM+hZIlLyQwK8vJgd/Bhjt1lNIz06T8FahwliRmMrY=";
        };

        completionShells = [
          "bash"
          "zsh"
          "fish"
        ];

        # Needed to build at all on a Mac; see the note beside their use.
        darwinBuildInputs = lib.optionals pkgs.stdenv.isDarwin [
          pkgs.apple-sdk_15
          pkgs.xcbuild
          pkgs.cctools
        ];

        pty = pkgs.rustPlatform.buildRustPackage {
          pname = "pty";
          inherit version;
          src = self;

          # No git dependencies in the lockfile, so it pins every input on its
          # own; nothing to hand-patch when a dependency bumps.
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [
            pkgs.installShellFiles
            pkgs.zig_0_15
          ]
          # Darwin only, and each one removes a named failure. Measured on
          # Apple silicon on 2026-09-02, added one at a time:
          #
          #   nothing          `DarwinSdkNotFound`
          #   + the first two  `libtool: FileNotFound`
          #   + all three      builds
          #
          # `DarwinSdkNotFound` is not Zig failing against a recent macOS. It
          # is Ghostty's own `build.zig` asking for a NATIVE libc installation
          # inside a sandbox that has none (`SharedDeps.zig` `findNative`, by
          # way of its bench target). No build flag avoids it: `build.zig`
          # constructs the bench target unconditionally and only gates
          # installing it. So the SDK has to be present rather than skipped.
          #
          # `apple-sdk_15` is what was proved to work, and it resolves to an
          # SDK that reports 14.4. **`apple-sdk_26` is untested**, and the
          # native failure this replaced tracked an SDK version of 26.x, so
          # the version here may be load-bearing rather than incidental. Do
          # not raise it without building on a Mac.
          #
          # **`xcbuild` is here for a reason that may not last, and the reason
          # is worth reading before anyone removes it.** On a Mac's own shell,
          # isolated one input at a time, it is the only one of the three that
          # makes a plain `zig build` link at all — and it works by BREAKING
          # SDK detection. nixpkgs' `xcrun` cannot find an SDK, so Zig stops
          # looking at the host's Xcode SDK and falls back to its own bundled
          # libSystem stub; the emitted link line carries no `-syslibroot`.
          # **If nixpkgs' `xcbuild` ever gains working SDK detection this
          # breaks again**, and the failure will look like a Zig-versus-macOS
          # problem rather than a packaging one, which is how it cost somebody
          # an afternoon the first time.
          ++ darwinBuildInputs;

          # zig's setup hook would otherwise replace cargo's build, check, and
          # install phases; zig is only here for libghostty-vt-sys's build script.
          dontUseZigBuild = true;
          dontUseZigCheck = true;
          dontUseZigInstall = true;

          env = {
            PTY_BUILD_SHA = buildSha;
            GHOSTTY_SOURCE_DIR = "${ghosttySrc}";
            GHOSTTY_ZIG_SYSTEM_DIR = "${ghosttyZigDeps}";
          };

          # Completions are the files vendored from the Node repo, which the
          # binary embeds and prints from `pty completions <shell>`;
          # `checks.completions` proves the two stay identical.
          postInstall = ''
            installShellCompletion --cmd pty \
              --bash completions/pty.bash \
              --zsh completions/pty.zsh \
              --fish completions/pty.fish
          '';

          # What the sandbox can prove, and it is deliberately less than a
          # machine can.
          #
          # A build sandbox has no Node `pty` to compare against, and it is not
          # a reliable place to start processes and wait on their output. Three
          # separate suites failed there on 2026-08-31 while passing on a
          # machine every time, in both build profiles: the daemon geometry
          # cases timed out waiting for a frame, a write to a daemon got a
          # broken pipe, and a terminal snapshot test raced its own shell loop.
          # None of those was a defect in the software.
          #
          # So the check phase runs `pty-core` and `pty-client`: the registry,
          # the protocol, the events log, the key and input parsers and the
          # client operations — everything the port gets wrong quietly. The
          # client tests talk to scripted fake daemons, not real ones. The behaviour
          # of the INSTALLED binary is proved by the checks below instead,
          # which is a better test of a package anyway.
          #
          # Everything runs on a machine with `cargo test --workspace`, and
          # `scripts/conformance-both.sh` runs the side-by-side against Node.
          cargoTestFlags = [ "-p" "pty-core" "-p" "pty-client" ];

          # The testkit's line-editing tests drive readline through `bash`;
          # stdenv's bash is built without it, so the interactive one goes first
          # on the check PATH.
          nativeCheckInputs = [
            pkgs.bashInteractive
          ];

          # THERE IS DELIBERATELY NO `ps` HERE, AND THAT IS THE SECOND
          # DECISION ON IT RATHER THAN THE FIRST.
          #
          # A build sandbox has none, and three things in this port shell out
          # to one on a Mac: the process start token (`-o lstart=`), whether a
          # process has been reaped (`-o stat=`) and a session's memory and
          # CPU (`-o rss=,pcpu=`).
          #
          # `pkgs.darwin.ps` was added on 2026-09-02 to cover them and taken
          # out the same day, because it gets one of the three right. It is
          # entitlement-limited: `rss` is refused outright, the state field
          # comes back blank for a live process, and only `lstart` is
          # correct. That made the check phase fail confusingly instead of
          # obviously, and it defeated a fix for the memory reading while
          # appearing to test it.
          #
          # **So the tests that need a working `ps` ask whether they have one
          # and say when they do not.** A skip that names its reason is worth
          # more than a green that measured nothing, and more than a red that
          # blames the code.
          #
          # Anything that finds a `ps` for darwin with the entitlements to
          # answer `rss` and `stat` can put it back and delete this.
          # `/bin/ps` on a real Mac answers all three.

          preCheck = ''
            export TMPDIR=$(mktemp -d /tmp/pty.XXXXXX)
            export HOME=$(mktemp -d)
          '';

          meta = {
            description = "Persistent terminal sessions with detach/attach, hosted by a per-session daemon";
            homepage = "https://github.com/compoundingtech/pty-rust";
            license = lib.licenses.mit;
            mainProgram = "pty";
          };
        };

        # Fleet consumers need the deterministic liveness faults in addition to
        # the sandbox-safe pty-core package check. Keep this as an exported check
        # so downstream flakes can depend on behavior without importing test internals.
        ptyFleetLiveness = pty.overrideAttrs (_: {
          pname = "pty-fleet-liveness-check";
          cargoTestFlags = [
            "-p"
            "pty-conformance"
            "--test"
            "list_liveness_budget"
          ];
          preCheck = ''
            export TMPDIR=$(mktemp -d /tmp/pty.XXXXXX)
            export HOME=$(mktemp -d)
            export PTY_TEST_BIN="$PWD/target/${pkgs.stdenv.hostPlatform.config}/release/pty"
          '';
        });
      in
      {
        packages.pty = pty;
        packages.default = pty;

        # `nix flake check` builds the package, its pty-core tests, and the
        # installed-binary smoke checks below.
        checks.pty = pty;
        checks.fleet-liveness = ptyFleetLiveness;

        # The installed completion files are the ones the binary prints, byte
        # for byte. Both come from completions/ at the repo root; this proves the
        # embedded copies did not drift from the files.
        checks.completions = pkgs.runCommand "pty-completions-${version}" { } ''
          ${lib.concatMapStringsSep "\n" (shell: ''
            ${pty}/bin/pty completions ${shell} > ${shell}.out
            cmp ${shell}.out ${self}/completions/pty.${shell} \
              || { echo "pty completions ${shell} differs from completions/pty.${shell}" >&2; exit 1; }
          '') completionShells}
          touch $out
        '';

        # Git-style forwarding finds `pty-<cmd>` on PATH. This ran `which`
        # once, and a sandbox has no `which`, so every extension read as an
        # unknown command wherever that program is absent. The bug was silent
        # and this is where it would have been caught, so it is pinned against
        # the installed binary rather than in a unit test.
        checks.extension-forwarding = pkgs.runCommand "pty-extension-forwarding-${version}" { } ''
          mkdir -p ext
          printf '#!/bin/sh\necho "forwarded: $*"\nexit 7\n' > ext/pty-hello
          chmod +x ext/pty-hello
          export PATH=$PWD/ext:$PATH
          export PTY_ROOT=$(mktemp -d)
          set +e
          # Not `out`: that is the output path this derivation must produce.
          got=$(${pty}/bin/pty hello world 2>&1)
          code=$?
          set -e
          [ "$code" = 7 ] || { echo "expected exit 7 from the extension, got $code: $got" >&2; exit 1; }
          case "$got" in
            *"forwarded: world"*) ;;
            *) echo "extension was not run: $got" >&2; exit 1 ;;
          esac
          touch $out
        '';

        # The built binary runs, prints the vendored usage text, and carries
        # this flake's commit in its version.
        checks.help = pkgs.runCommand "pty-help-${version}" { } ''
          ${pty}/bin/pty help > help.out
          cmp help.out ${self}/crates/pty/tests/fixtures/help/usage.txt \
            || { echo "pty help differs from the usage fixture" >&2; exit 1; }
          ${pty}/bin/pty version > version.out
          grep -qx '${version}+${buildSha}' version.out \
            || { echo "pty version printed $(cat version.out), expected ${version}+${buildSha}" >&2; exit 1; }
          touch $out
        '';

        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.clippy
            pkgs.git
            pkgs.rust-analyzer
            pkgs.rustc
            pkgs.rustfmt
            pkgs.zig_0_15

            # The package build has this and the shell did not, so a
            # `cargo test --workspace` in here failed two line-editing tests
            # for a reason that had nothing to do with the code: the testkit
            # drives readline through `bash`, and stdenv's bash is built
            # without it.
            pkgs.bashInteractive
          ]
          # Likewise: without these a `cargo test --workspace` in this shell
          # does not get as far as the tests on a Mac. It fails building
          # libghostty.
          ++ darwinBuildInputs;

          # `nix develop` appends `nix-shell.XXXXXX` to `TMPDIR`, and on a
          # Mac that pushes a session's socket path past the 104-byte kernel
          # limit: 172 tests failed for that reason alone on 2026-09-02, in
          # this very shell. The rigs now say why, but the shell should not
          # walk anyone into it in the first place.
          shellHook = ''
            export TMPDIR=''${PTY_DEV_TMPDIR:-/tmp}
          '';

          # The same pre-fetched Ghostty as the package, so a `cargo build` in
          # this shell fetches nothing.
          env = {
            GHOSTTY_SOURCE_DIR = "${ghosttySrc}";
            GHOSTTY_ZIG_SYSTEM_DIR = "${ghosttyZigDeps}";
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
          };
        };
      }
    );
}
