{
  description = "kernel-node development environment";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
      forAllSystems = f:
        nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            rustc
            cargo
            clippy
            rustfmt
            clang_19
            llvmPackages_19.libclang
            cmake
            ninja
            gnumake
            gcc14
            pkg-config
            capnproto
            bitcoind
          ];

          buildInputs = with pkgs; [
            boost
          ];

          LIBCLANG_PATH = "${pkgs.llvmPackages_19.libclang.lib}/lib";
          BITCOIND_EXE = "${pkgs.bitcoind}/bin/bitcoind";

          shellHook = ''
            export CC=clang
            export CXX=clang++
            export CMAKE_GENERATOR=Ninja
          '';
        };
      });
    };
}
