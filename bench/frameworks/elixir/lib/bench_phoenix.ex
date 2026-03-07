defmodule BenchPhoenix.Application do
  use Application

  @impl true
  def start(_type, _args) do
    port = String.to_integer(System.get_env("PORT") || "3007")

    children = [
      {Plug.Cowboy, scheme: :http, plug: BenchPhoenix.Router, options: [port: port]}
    ]

    IO.puts("Plug/Cowboy listening on 0.0.0.0:#{port}")
    Supervisor.start_link(children, strategy: :one_for_one)
  end
end

defmodule BenchPhoenix.Data do
  @worlds for i <- 1..10_000, do: %{id: i, randomNumber: :rand.uniform(10_000)}
  @fortunes [
    %{id: 1,  message: "fortune: No such file or directory"},
    %{id: 2,  message: "A computer scientist is someone who fixes things that aren't broken."},
    %{id: 3,  message: "After all is said and done, more is said than done."},
    %{id: 4,  message: "Any program that runs right is obsolete."},
    %{id: 5,  message: "A list is only as strong as its weakest link. — Donald Knuth"},
    %{id: 6,  message: "Feature: A bug with seniority."},
    %{id: 7,  message: "Computers make very fast, very accurate mistakes."},
    %{id: 8,  message: ~S(<script>alert("This should not be displayed in a browser alert box.");</script>)},
    %{id: 9,  message: "A computer program does what you tell it to do, not what you want it to do."},
    %{id: 10, message: "If Java had true garbage collection, most programs would delete themselves upon execution."},
    %{id: 11, message: "フレームワークのベンチマーク"},
    %{id: 12, message: "The best thing about a boolean is even if you are wrong, you are only off by a bit."},
  ]
  @users for i <- 1..1000, do: %{id: i, name: "User #{i}", email: "user#{i}@example.com", role: if(rem(i, 10) == 0, do: "admin", else: "member")}

  def worlds, do: @worlds
  def fortunes, do: @fortunes
  def users, do: @users
  def random_world, do: Enum.random(@worlds)

  def parse_count(nil), do: 1
  def parse_count(val) do
    case Integer.parse(val) do
      {n, _} -> n |> max(1) |> min(500)
      :error -> 1
    end
  end
end

defmodule BenchPhoenix.Router do
  use Plug.Router

  plug :match
  plug :dispatch

  # ─── TechEmpower ───────────────────────────────────────────────

  get "/plaintext" do
    conn |> put_resp_content_type("text/plain") |> send_resp(200, "Hello, World!")
  end

  get "/json" do
    conn |> put_resp_content_type("application/json") |> send_resp(200, Jason.encode!(%{message: "Hello, World!"}))
  end

  get "/db" do
    conn |> put_resp_content_type("application/json") |> send_resp(200, Jason.encode!(BenchPhoenix.Data.random_world()))
  end

  get "/queries" do
    conn = Plug.Conn.fetch_query_params(conn)
    count = BenchPhoenix.Data.parse_count(conn.query_params["queries"])
    results = for _ <- 1..count, do: BenchPhoenix.Data.random_world()
    conn |> put_resp_content_type("application/json") |> send_resp(200, Jason.encode!(results))
  end

  get "/fortunes" do
    rows = BenchPhoenix.Data.fortunes()
    |> Enum.map(fn f -> {f.id, f.message} end)
    |> Kernel.++([{0, "Additional fortune added at request time."}])
    |> Enum.sort_by(fn {_, msg} -> msg end)

    html = [
      "<!DOCTYPE html><html><head><title>Fortunes</title></head><body><table><tr><th>id</th><th>message</th></tr>",
      Enum.map(rows, fn {id, msg} ->
        ["<tr><td>", Integer.to_string(id), "</td><td>", Plug.HTML.html_escape_to_iodata(msg), "</td></tr>"]
      end),
      "</table></body></html>"
    ]
    conn |> put_resp_content_type("text/html; charset=utf-8") |> send_resp(200, html)
  end

  get "/updates" do
    conn = Plug.Conn.fetch_query_params(conn)
    count = BenchPhoenix.Data.parse_count(conn.query_params["queries"])
    results = for _ <- 1..count do
      w = BenchPhoenix.Data.random_world()
      %{w | randomNumber: :rand.uniform(10_000)}
    end
    conn |> put_resp_content_type("application/json") |> send_resp(200, Jason.encode!(results))
  end

  get "/cached-queries" do
    conn = Plug.Conn.fetch_query_params(conn)
    count = BenchPhoenix.Data.parse_count(conn.query_params["count"])
    results = for _ <- 1..count, do: BenchPhoenix.Data.random_world()
    conn |> put_resp_content_type("application/json") |> send_resp(200, Jason.encode!(results))
  end

  # ─── REST API ──────────────────────────────────────────────────

  get "/api/users/:id" do
    uid = String.to_integer(id)
    case Enum.find(BenchPhoenix.Data.users(), fn u -> u.id == uid end) do
      nil -> conn |> put_resp_content_type("application/json") |> send_resp(404, ~S({"error":"not found"}))
      user -> conn |> put_resp_content_type("application/json") |> send_resp(200, Jason.encode!(user))
    end
  end

  post "/api/echo" do
    {:ok, body, conn} = Plug.Conn.read_body(conn)
    conn |> put_resp_content_type("application/json") |> send_resp(200, body)
  end

  get "/api/search" do
    conn = Plug.Conn.fetch_query_params(conn)
    q = conn.query_params["q"] || ""
    page = case Integer.parse(conn.query_params["page"] || "") do
      {n, _} when n >= 1 -> n
      _ -> 1
    end
    limit = min(BenchPhoenix.Data.parse_count(conn.query_params["limit"]) |> max(1), 100)
    offset = (page - 1) * limit
    results = BenchPhoenix.Data.users()
    |> Enum.filter(fn u -> q == "" or String.contains?(u.name, q) end)
    |> Enum.drop(offset)
    |> Enum.take(limit)
    conn |> put_resp_content_type("application/json") |> send_resp(200, Jason.encode!(results))
  end

  # ─── Browser / misc ────────────────────────────────────────────

  get "/browser/page" do
    conn |> put_resp_content_type("text/html; charset=utf-8") |> send_resp(200, "<html><body><h1>Hello</h1></body></html>")
  end

  get "/redirect" do
    conn |> put_resp_header("location", "/plaintext") |> send_resp(302, "")
  end

  match _ do
    send_resp(conn, 404, "Not Found")
  end
end
