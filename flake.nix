{
  description = "pty - Persistent terminal sessions with detach/attach support";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      # Systems we support
      supportedSystems = [ "aarch64-darwin" "x86_64-darwin" "x86_64-linux" "aarch64-linux" ];

      # Helper to create outputs for each system
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;

      # Get pkgs for a given system
      pkgsFor = system: nixpkgs.legacyPackages.${system};
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = pkgsFor system;

          pty = pkgs.buildNpmPackage {
            pname = "pty";
            version = "0.1.0";

            src = ./.;

            # Generated from package-lock.json.
            # Regenerate with: nix run nixpkgs#prefetch-npm-deps -- package-lock.json
            npmDepsHash = "sha256-+65s9CLTJNlPt82HKOkngir4KZF1yCy96hderY0m2qQ=";

            # node-pty has native code that needs these at build time
            nativeBuildInputs = with pkgs; [ python3 pkg-config ];
            buildInputs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
              pkgs.apple-sdk_15
            ];

            # No compile step — Node runs TypeScript directly via type stripping.
            dontNpmBuild = true;

            # Install outside of node_modules so Node's TypeScript stripping works.
            # (Node refuses to strip types for files under node_modules/)
            installPhase = ''
              runHook preInstall

              mkdir -p $out/lib/pty
              cp -r . $out/lib/pty

              # node-pty's spawn-helper must be executable
              chmod +x $out/lib/pty/node_modules/node-pty/prebuilds/*/spawn-helper 2>/dev/null || true

              mkdir -p $out/bin
              ln -s $out/lib/pty/bin/pty $out/bin/pty
              chmod +x $out/bin/pty
              substituteInPlace $out/bin/pty \
                --replace-fail "#!/usr/bin/env node" "#!${pkgs.nodejs}/bin/node"

              installShellCompletion --bash completions/pty.bash
              installShellCompletion --zsh completions/pty.zsh
              installShellCompletion --fish completions/pty.fish

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
          default = pty;
          inherit pty;
        }
      );

      # `nix develop` — gives you node, npm, and project deps for hacking
      devShells = forAllSystems (system:
        let pkgs = pkgsFor system;
        in {
          default = pkgs.mkShell {
            packages = with pkgs; [
              nodejs_22
              python3    # for node-pty native build
              pkg-config
            ];
          };
        }
      );
    };
}
