# Minimal Rails-like app using Action Dispatch
# Run with: bundle exec puma -p 3006 -w 4 -t 8:8 rails_app.ru
#
# Requires: gem install rails
# This gives a realistic Rails overhead comparison.

require "action_dispatch"
require "json"

class BenchApp < ActionDispatch::Routing::RouteSet
  def initialize
    super
    draw do
      get "/plaintext", to: ->(env) {
        [200, { "content-type" => "text/plain" }, ["Hello, World!"]]
      }
      get "/json", to: ->(env) {
        [200, { "content-type" => "application/json" }, [{ message: "Hello, World!" }.to_json]]
      }
    end
  end
end

run BenchApp.new
