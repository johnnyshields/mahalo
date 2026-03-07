# Minimal Rack app for benchmarking
# Run with: bundle exec puma -p 3005 -w 4 -t 8:8

require "json"

WORLDS = (1..10_000).map { |i| { id: i, randomNumber: rand(1..10_000) } }.freeze
FORTUNES = [
  { id: 1,  message: "fortune: No such file or directory" },
  { id: 2,  message: "A computer scientist is someone who fixes things that aren't broken." },
  { id: 3,  message: "After all is said and done, more is said than done." },
  { id: 4,  message: "Any program that runs right is obsolete." },
  { id: 5,  message: "A list is only as strong as its weakest link. — Donald Knuth" },
  { id: 6,  message: "Feature: A bug with seniority." },
  { id: 7,  message: "Computers make very fast, very accurate mistakes." },
  { id: 8,  message: '<script>alert("This should not be displayed in a browser alert box.");</script>' },
  { id: 9,  message: "A computer program does what you tell it to do, not what you want it to do." },
  { id: 10, message: "If Java had true garbage collection, most programs would delete themselves upon execution." },
  { id: 11, message: "フレームワークのベンチマーク" },
  { id: 12, message: "The best thing about a boolean is even if you are wrong, you are only off by a bit." },
].freeze
USERS = (1..1000).map { |i| { id: i, name: "User #{i}", email: "user#{i}@example.com", role: i % 10 == 0 ? "admin" : "member" } }.freeze

def parse_count(val)
  n = val.to_i
  n < 1 ? 1 : (n > 500 ? 500 : n)
end

def escape_html(s)
  s.gsub("&", "&amp;").gsub("<", "&lt;").gsub(">", "&gt;").gsub('"', "&quot;").gsub("'", "&#x27;")
end

def parse_query(qs)
  return {} if qs.nil? || qs.empty?
  qs.split("&").each_with_object({}) { |pair, h| k, v = pair.split("=", 2); h[k] = v }
end

app = lambda do |env|
  path = env["PATH_INFO"]
  qs = parse_query(env["QUERY_STRING"])

  case path
  when "/plaintext"
    [200, { "content-type" => "text/plain" }, ["Hello, World!"]]
  when "/json"
    [200, { "content-type" => "application/json" }, [{ message: "Hello, World!" }.to_json]]
  when "/db"
    [200, { "content-type" => "application/json" }, [WORLDS.sample.to_json]]
  when "/queries"
    count = parse_count(qs["queries"] || "1")
    results = count.times.map { WORLDS.sample }
    [200, { "content-type" => "application/json" }, [results.to_json]]
  when "/fortunes"
    rows = FORTUNES.map { |f| [f[:id], f[:message]] }
    rows << [0, "Additional fortune added at request time."]
    rows.sort_by! { |r| r[1] }
    html = '<!DOCTYPE html><html><head><title>Fortunes</title></head><body><table><tr><th>id</th><th>message</th></tr>'
    rows.each { |id, msg| html << "<tr><td>#{id}</td><td>#{escape_html(msg)}</td></tr>" }
    html << "</table></body></html>"
    [200, { "content-type" => "text/html; charset=utf-8" }, [html]]
  when "/updates"
    count = parse_count(qs["queries"] || "1")
    results = count.times.map { w = WORLDS.sample.dup; w[:randomNumber] = rand(1..10_000); w }
    [200, { "content-type" => "application/json" }, [results.to_json]]
  when "/cached-queries"
    count = parse_count(qs["count"] || "1")
    results = count.times.map { WORLDS.sample }
    [200, { "content-type" => "application/json" }, [results.to_json]]
  when %r{^/api/users/(\d+)$}
    id = $1.to_i
    user = USERS.find { |u| u[:id] == id }
    user ? [200, { "content-type" => "application/json" }, [user.to_json]] :
           [404, { "content-type" => "application/json" }, ['{"error":"not found"}']]
  when "/api/search"
    q = qs["q"] || ""
    page = (qs["page"] || "1").to_i
    limit = [(qs["limit"] || "20").to_i, 100].min
    offset = (page - 1) * limit
    results = USERS.select { |u| q.empty? || u[:name].include?(q) }.slice(offset, limit) || []
    [200, { "content-type" => "application/json" }, [results.to_json]]
  when "/browser/page"
    [200, { "content-type" => "text/html; charset=utf-8" }, ["<html><body><h1>Hello</h1></body></html>"]]
  when "/redirect"
    [302, { "location" => "/plaintext" }, [""]]
  else
    [404, { "content-type" => "text/plain" }, ["Not Found"]]
  end
end

run app
