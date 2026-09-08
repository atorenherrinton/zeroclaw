const ready = document.__zeroclawNavigationMarker !== args.marker ||
  (args.fragment_only === true && location.href === args.url);
return JSON.stringify({ready, reasons: ready ? [] : ['previous_document_still_active']});
