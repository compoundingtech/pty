{
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
        pty = pkgs.buildNpmPackage {
          pname = "pty";
          version = "0.1.0";
          src = self;

          npmDepsHash = "sha256-xCWanXIKz2oqsii5kEK4RWhVROOkTPqitB2zRwbJObs=";

          # The CLI runs TypeScript via tsx at runtime — no build step needed
          dontBuild = true;

          installPhase = ''
            runHook preInstall

            mkdir -p $out/lib/pty
            cp -r . $out/lib/pty

            mkdir -p $out/bin
            ln -s $out/lib/pty/bin/pty $out/bin/pty
            chmod +x $out/bin/pty

            substituteInPlace $out/bin/pty \
              --replace-fail "#!/usr/bin/env node" "#!${pkgs.nodejs}/bin/node"

            # Shell completions
            install -Dm644 completions/pty.bash $out/share/bash-completion/completions/pty
            install -Dm644 completions/pty.zsh $out/share/zsh/site-functions/_pty
            install -Dm644 completions/pty.fish $out/share/fish/vendor_completions.d/pty.fish

            runHook postInstall
          '';

          meta = with pkgs.lib; {
            description = "Persistent terminal sessions with detach/attach support";
            homepage = "https://github.com/myobie/pty";
            license = licenses.mit;
            mainProgram = "pty";
          };
        };
      in
      {
        packages = {
          default = pty;
          inherit pty;
        };
      }
    );
}
