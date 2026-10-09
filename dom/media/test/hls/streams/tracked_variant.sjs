// Serves a multivariant playlist whose first variant, with the codecs and
// bandwidth given in the query string, has segments that record whether they
// were ever requested. The other variants are playable H.264. Used to check
// that the first variant is never played.
function getParams(queryString) {
  const params = {};
  for (const pair of queryString.split("&")) {
    const [name, value] = pair.split("=");
    params[name] = decodeURIComponent(value);
  }
  return params;
}

function handleRequest(request, response) {
  const params = getParams(request.queryString);
  const stateKey = `hls-tracked-${params.key}`;
  response.setHeader("Cache-Control", "no-store", false);
  if (params.check !== undefined) {
    response.setHeader("Content-Type", "text/plain", false);
    response.write(getState(stateKey) || "0");
    return;
  }
  if (params.segment !== undefined) {
    setState(stateKey, "1");
    response.setStatusLine(request.httpVersion, 404, "Not Found");
    return;
  }
  response.setHeader("Content-Type", "application/vnd.apple.mpegurl", false);
  if (params.variant !== undefined) {
    response.write(
      "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-VERSION:3\n" +
        "#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n" +
        `#EXTINF:5.6067,\ntracked_variant.sjs?key=${params.key}&segment=0\n` +
        `#EXTINF:4.37,\ntracked_variant.sjs?key=${params.key}&segment=1\n` +
        "#EXT-X-ENDLIST\n"
    );
    return;
  }
  let body =
    "#EXTM3U\n" +
    `#EXT-X-STREAM-INF:BANDWIDTH=${params.bandwidth},CODECS="${params.codecs}",RESOLUTION=400x300\n` +
    `tracked_variant.sjs?key=${params.key}&variant=1\n` +
    '#EXT-X-STREAM-INF:BANDWIDTH=232370,CODECS="mp4a.40.2,avc1.4d4015",RESOLUTION=400x300\n' +
    "av.m3u8\n";
  if (params.avcVariants == "2") {
    body +=
      '#EXT-X-STREAM-INF:BANDWIDTH=400000,CODECS="mp4a.40.2,avc1.4d4015",RESOLUTION=400x300\n' +
      "sub/av.m3u8\n";
  }
  response.write(body);
}
