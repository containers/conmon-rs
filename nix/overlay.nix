let
  static = import ./static.nix;
in
self: super:
{
  libunwind = (static super.libunwind);
}
