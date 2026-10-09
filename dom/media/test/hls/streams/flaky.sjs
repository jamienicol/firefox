// Serves a media playlist whose segments fail with a 503 the first two times
// they are requested.
const FAILURES = 2;

function getParams(queryString) {
  const params = {};
  for (const pair of queryString.split("&")) {
    const [name, value] = pair.split("=");
    params[name] = value;
  }
  return params;
}

function handleRequest(request, response) {
  const params = getParams(request.queryString);
  const key = params.key;
  const seg = params.seg;
  response.setHeader("Cache-Control", "no-store", false);
  if (seg === undefined) {
    response.setHeader("Content-Type", "application/vnd.apple.mpegurl", false);
    response.write(
      "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-VERSION:3\n" +
        "#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n" +
        `#EXTINF:5.6067,\nflaky.sjs?key=${key}&seg=0\n` +
        `#EXTINF:4.37,\nflaky.sjs?key=${key}&seg=1\n` +
        "#EXT-X-ENDLIST\n"
    );
    return;
  }
  const stateKey = `hls-flaky-${key}-${seg}`;
  const count = parseInt(getState(stateKey) || "0", 10);
  setState(stateKey, String(count + 1));
  if (count < FAILURES) {
    response.setStatusLine(request.httpVersion, 503, "Service Unavailable");
    return;
  }
  response.setStatusLine(request.httpVersion, 302, "Found");
  response.setHeader("Location", `av_${seg}.ts`, false);
}
