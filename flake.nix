{
  description = "pty - persistent terminal sessions with detach/attach, in Rust on libghostty";

  nixConfig = {
    extra-substituters = [ "https://overeng-effect-utils.cachix.org" ];
    extra-trusted-public-keys = [
      "overeng-effect-utils.cachix.org-1:KFmqYNF6Q7ZzVYPl2znpJYZGEolage9YNCA9res6vKc="
    ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    # Generator tooling only. Its own nixpkgs is kept so the pty build toolchain
    # does not change and the published genie artifacts stay substitutable.
    effect-utils = {
      url = "github:overengineeringstudio/effect-utils/3089f7e1faa82d7a4cb4de0e8d485164f837708b";
      inputs.flake-utils.follows = "flake-utils";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      effect-utils,
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
        ghosttyContract = builtins.fromJSON (builtins.readFile ./libghostty-vt-contract.json);

        # The native artifact owns source and Zig-package fetching. Rust
        # consumers link its pkg-config archive and never invoke Zig.
        ghosttyRev = ghosttyContract.ghosttyRev;
        ghosttyShortRev = lib.substring 0 7 ghosttyRev;

        ghosttySrc = pkgs.fetchFromGitHub {
          name = "ghostty-${ghosttyShortRev}-src";
          owner = "ghostty-org";
          repo = "ghostty";
          rev = ghosttyRev;
          hash = ghosttyContract.sourceHash;
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
          outputHash = ghosttyContract.zigDepsHash;
        };

        completionShells = [
          "bash"
          "zsh"
          "fish"
        ];

        # Ghostty's native producer needs SDK discovery tools on Darwin.
        # Release runners use Namespace, but the build SDK remains this pinned
        # Nix SDK rather than the runner image's native SDK (q11).
        darwinBuildInputs = lib.optionals pkgs.stdenv.isDarwin [
          pkgs.apple-sdk_15
          pkgs.xcbuild
          pkgs.cctools
        ];

        # On Linux the target is named. Without `-Dtarget` Zig builds for
        # the host it runs on: the archive keeps that host's dynamic linker,
        # a /nix/store glibc path, in read-only data, and its code is tuned
        # to the build machine's CPU. A named target gives the standard
        # loader path and baseline code, so a substituted archive runs on any
        # machine of its architecture and carries no store path. Darwin needs
        # no flag: Ghostty already swaps a native macOS target for a generic
        # one (`genericMacOSTarget` in src/build/Config.zig).
        zigTargetFlags = lib.optionalString pkgs.stdenv.hostPlatform.isLinux
          "-Dtarget=${pkgs.stdenv.hostPlatform.parsed.cpu.name}-linux-gnu -Dcpu=baseline";

        libghostty-vt = assert ghosttyContract.zigVersion == pkgs.zig_0_15.version; pkgs.stdenv.mkDerivation {
          pname = "libghostty-vt";
          version = ghosttyContract.rustBindingsVersion;
          src = ghosttySrc;
          nativeBuildInputs = [ pkgs.zig_0_15 ] ++ darwinBuildInputs;
          dontConfigure = true;
          dontUseZigBuild = true;
          dontUseZigCheck = true;
          dontUseZigInstall = true;
          buildPhase = ''
            runHook preBuild
            export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-global-cache"
            zig build -j2 ${zigTargetFlags} -Demit-lib-vt=true -Doptimize=ReleaseFast \
              -Demit-xcframework=false -Dapp-runtime=none \
              --system ${ghosttyZigDeps} --prefix "$out"
            runHook postBuild
          '';
          dontInstall = true;
          # stdenv does not strip archive members by default. Their DWARF paths
          # otherwise retain Zig and the immutable Zig package cache at runtime.
          postFixup = ''
            strip -S "$out/lib/libghostty-vt.a"
            # Consumers that refuse any /nix/store string link these files
            # into their own artifacts. The pkg-config files under share/
            # name this output's prefix, which is their job.
            for lib in "$out"/lib/*; do
              if grep -q -a '/nix/store/' "$lib"; then
                echo "$lib contains a /nix/store path:" >&2
                grep -a -o '/nix/store/[[:graph:]]*' "$lib" | sort -u >&2
                exit 1
              fi
            done
            mkdir -p "$out/share/licenses/libghostty-vt"
            cp LICENSE "$out/share/licenses/libghostty-vt/LICENSE"
            cp ${./libghostty-vt-contract.json} "$out/share/libghostty-vt-contract.json"
          '';
          passthru = {
            inherit ghosttyRev;
            rustBindingsVersion = ghosttyContract.rustBindingsVersion;
            contract = ghosttyContract;
          };
          meta.license = lib.licenses.mit;
        };

        pty = pkgs.rustPlatform.buildRustPackage {
          pname = "pty";
          inherit version;
          src = self;

          # No git dependencies in the lockfile, so it pins every input on its
          # own; nothing to hand-patch when a dependency bumps.
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [
            pkgs.installShellFiles
            pkgs.pkg-config
          ];
          buildInputs = [ libghostty-vt ];

          env.PTY_BUILD_SHA = buildSha;

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
        packages.libghostty-vt = libghostty-vt;
        devShells.libghostty-consumer = pkgs.mkShell {
          packages = [ pkgs.cargo pkgs.rustc pkgs.pkg-config ] ++ lib.optionals pkgs.stdenv.isLinux [ pkgs.mold ];
          buildInputs = [ libghostty-vt ];
        };

        # `nix flake check` builds the package, its pty-core tests, and the
        # installed-binary smoke checks below.
        checks.pty = pty;
        checks.fleet-liveness = ptyFleetLiveness;
        checks.libghostty-contract =
          let
            lock = builtins.fromTOML (builtins.readFile ./Cargo.lock);
            sys = lib.findSingle (p: p.name == "libghostty-vt-sys")
              (throw "missing sys crate") (throw "multiple sys crates") lock.package;
            archive = pkgs.fetchurl {
              url = "https://static.crates.io/crates/libghostty-vt-sys/libghostty-vt-sys-${sys.version}.crate";
              sha256 = sys.checksum;
            };
          in pkgs.runCommand "libghostty-contract" {
            nativeBuildInputs = [ pkgs.python3 ];
          } ''
            tar -xf ${archive}
            python ${self}/scripts/check-libghostty-contract.py \
              --sys-source "$PWD/libghostty-vt-sys-${sys.version}"
            touch "$out"
          '';
        checks.libghostty-runtime-closure =
          let closure = pkgs.closureInfo { rootPaths = [ libghostty-vt ]; };
          in pkgs.runCommand "libghostty-runtime-closure" { } ''
            while IFS= read -r path; do
              case "$path" in
                *-zig-*|*-ghostty-*-src|*-ghostty-*-zig-deps)
                  echo "native runtime closure retains toolchain/source: $path" >&2
                  exit 1
                  ;;
              esac
            done < ${closure}/store-paths
            echo "PASS: native runtime closure contains no Zig/source/cache"
            touch "$out"
          '';

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
            pkgs.pkg-config

            # The package build has this and the shell did not, so a
            # `cargo test --workspace` in here failed two line-editing tests
            # for a reason that had nothing to do with the code: the testkit
            # drives readline through `bash`, and stdenv's bash is built
            # without it.
            pkgs.bashInteractive
          ];
          buildInputs = [ libghostty-vt ];

          # `nix develop` appends `nix-shell.XXXXXX` to `TMPDIR`, and on a
          # Mac that pushes a session's socket path past the 104-byte kernel
          # limit: 172 tests failed for that reason alone on 2026-09-02, in
          # this very shell. The rigs now say why, but the shell should not
          # walk anyone into it in the first place.
          shellHook = ''
            export TMPDIR=''${PTY_DEV_TMPDIR:-/tmp}
          '';

          env = {
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
          };
        };

        # Keep generator tooling out of the Rust build and test shell.
        devShells.genie = pkgs.mkShell {
          packages = [ effect-utils.packages.${system}.genie ];
          shellHook = ''
            mkdir -p repos
            ln -sfn ${effect-utils} repos/effect-utils
          '';
        };
      }
    ) // {
      lib.libghosttyContract = builtins.fromJSON (builtins.readFile ./libghostty-vt-contract.json);
      overlays.default = final: prev: {
        libghostty-vt = self.packages.${prev.stdenv.hostPlatform.system}.libghostty-vt;
      };
    };
}
