// A native URL preflight has already passed policy. Recheck in this exact
// document immediately before its first mutation, guarding navigation races
// and Safari's native URL changing before the old document is replaced.
if (!args.url || location.href !== new URL(args.url).href) {
  throw new Error('The dedicated Safari page changed before navigation; read its current state before retrying');
}
document.__zeroclawNavigationMarker = args.marker;
return JSON.stringify({url: location.href});
