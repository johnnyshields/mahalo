# Minimal Rack app for benchmarking
# Run with: bundle exec puma -p 3005 -w 4 -t 8:8

require "json"

app = lambda do |env|
  case env["PATH_INFO"]
  when "/plaintext"
    [200, { "content-type" => "text/plain" }, ["Hello, World!"]]
  when "/json"
    [200, { "content-type" => "application/json" }, [{ message: "Hello, World!" }.to_json]]
  else
    [404, { "content-type" => "text/plain" }, ["Not Found"]]
  end
end

run app
