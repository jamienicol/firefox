function handleRequest(request, response) {
  response.setStatusLine(request.httpVersion, 302, "Found");
  response.setHeader("Location", "sub/av.m3u8", false);
  response.setHeader("Cache-Control", "no-store", false);
}
