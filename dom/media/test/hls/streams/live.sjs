// Serves a sliding-window live playlist that loops av_0.ts and av_1.ts, with a
// discontinuity at the start of each loop. The window advances in real time
// from the start time passed in the query string.
const DURATIONS = [5.6067, 4.37];
const WINDOW = 4;
const AHEAD = 3;

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
  const elapsed = (Date.now() - parseInt(params.start, 10)) / 1000;
  const loop = DURATIONS[0] + DURATIONS[1];
  const current = Math.floor(elapsed / loop) * 2 + (elapsed % loop >= DURATIONS[0] ? 1 : 0);
  const last = current + AHEAD;
  const first = Math.max(0, last - WINDOW + 1);
  let body =
    "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-VERSION:3\n" +
    `#EXT-X-MEDIA-SEQUENCE:${first}\n` +
    `#EXT-X-DISCONTINUITY-SEQUENCE:${Math.floor(first / 2)}\n`;
  for (let n = first; n <= last; n++) {
    if (n > first && n % 2 == 0) {
      body += "#EXT-X-DISCONTINUITY\n";
    }
    body += `#EXTINF:${DURATIONS[n % 2]},\nav_${n % 2}.ts\n`;
  }
  response.setHeader("Content-Type", "application/vnd.apple.mpegurl", false);
  response.setHeader("Cache-Control", "no-store", false);
  response.write(body);
}
