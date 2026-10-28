let
  isNixheal = o: o.__nixheal or false;
  before =
    list:
    if builtins.length list == 0 || isNixheal (builtins.head list) then
      [ ]
    else
      [ (builtins.head list) ] ++ before (builtins.tail list);
in
{
  inherit before;
  wrap = mkOverlay: carry: args: {
    __nixheal = true;
    __functor =
      _: final: prev:
      mkOverlay (args // { overlays = if carry then before (prev.overlays or [ ]) else [ ]; }) final prev;
  };
}
