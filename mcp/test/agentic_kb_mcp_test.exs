defmodule AgenticKbMcp.RenderFixture do
  def entry(overrides \\ %{}) do
    Map.merge(
      %{
        "id" => "ent-001",
        "path" => "elixir/mcp/render-contract",
        "summary" => "Renderer contract fixture",
        "content" => "Body line 1\nBody line 2",
        "score" => 0.98765,
        "confidence" => 0.83,
        "audit_n" => 7,
        "tags" => ["elixir", "mcp"],
        "source" => "kb_search",
        "score_kind" => "hybrid",
        "origin_repo" => "/tmp/peer-repo",
        "evidence" => [
          %{
            "id" => "ev-1",
            "kind" => "code",
            "citation_path" => "lib/foo.ex:10-14",
            "citation_sha" => "sha-1",
            "citation_hash" => "hash-1",
            "citation_excerpt" => "<<UNTRUSTED_EXCERPT>>alpha<<END>>",
            "verified" => true
          },
          %{
            "id" => "ev-2",
            "kind" => "doc",
            "citation_path" => "README.md:2-4",
            "citation_sha" => "sha-2",
            "citation_hash" => "hash-2",
            "citation_excerpt" => "<<UNTRUSTED_EXCERPT>>beta<<END>>",
            "verified" => false
          },
          %{
            "id" => "ev-3",
            "kind" => "note",
            "citation_path" => "notes.md:7-9",
            "citation_sha" => "sha-3",
            "citation_hash" => "hash-3",
            "citation_excerpt" => "<<UNTRUSTED_EXCERPT>>gamma<<END>>",
            "verified" => nil
          },
          %{
            "id" => "ev-4",
            "kind" => "note",
            "citation_path" => "notes.md:10-12",
            "citation_sha" => "sha-4",
            "citation_hash" => "hash-4",
            "citation_excerpt" => "<<UNTRUSTED_EXCERPT>>delta<<END>>",
            "verified" => nil
          }
        ]
      },
      overrides
    )
  end

  def long_entry(id, content_size) do
    entry(%{
      "id" => id,
      "path" => "elixir/mcp/#{id}",
      "summary" => "Summary #{id}",
      "content" => String.duplicate("x", content_size),
      "evidence" => []
    })
  end
end

defmodule AgenticKbMcpTest do
  use ExUnit.Case, async: true

  alias AgenticKbMcp.McpServer
  alias AgenticKbMcp.RenderFixture

  @documented_omissions MapSet.new([
                          "origin_repo",
                          "score_kind",
                          "source",
                          "tags",
                          "evidence.id",
                          "evidence.citation_excerpt",
                          "evidence.citation_hash",
                          "evidence.citation_sha"
                        ])

  test "known entry renders id, confidence and compact evidence lines" do
    output = McpServer.format_entries([RenderFixture.entry()])

    assert output =~ "## elixir/mcp/render-contract (score: 0.988)"
    assert output =~ "id: ent-001"
    assert output =~ "confidence: 0.83  audit_n: 7"
    assert output =~ "evidence:"
    assert output =~ "- kind=code  citation_path=lib/foo.ex:10-14  verified=verified"
    assert output =~ "- kind=doc  citation_path=README.md:2-4  verified=BROKEN"
    assert output =~ "- kind=note  citation_path=notes.md:7-9  verified=deferred"
    refute output =~ "notes.md:10-12"
    refute output =~ "<<UNTRUSTED_EXCERPT>>"
    refute output =~ "sha-1"
    refute output =~ "hash-1"
  end

  test "empty evidence renders no stanza" do
    output =
      RenderFixture.entry(%{"id" => "ent-empty", "evidence" => []})
      |> then(&McpServer.format_entries([&1]))

    assert output =~ "id: ent-empty"
    refute output =~ "evidence:"
  end

  test "bytes ceiling truncates at whole-entry boundary and appends omitted count" do
    entries = [
      RenderFixture.long_entry("ent-a", 14_000),
      RenderFixture.long_entry("ent-b", 14_000),
      RenderFixture.long_entry("ent-c", 14_000)
    ]

    output = McpServer.format_entries(entries)

    assert output =~ "id: ent-a"
    assert output =~ "id: ent-b"
    refute output =~ "id: ent-c"
    assert output =~ "…(1 more entries omitted)"
    assert byte_size(output) <= 32_000
  end

  test "fixture field set is fully rendered or explicitly documented as omitted" do
    fixture = RenderFixture.entry()
    output = McpServer.format_entries([fixture])

    rendered_fields =
      [
        {"id", "id: #{fixture["id"]}"},
        {"path", fixture["path"]},
        {"summary", fixture["summary"]},
        {"content", "Body line 1"},
        {"score", "(score: 0.988)"},
        {"confidence", "confidence:"},
        {"audit_n", "audit_n:"},
        {"evidence", "evidence:"},
        {"evidence.kind", "kind="},
        {"evidence.citation_path", "citation_path="},
        {"evidence.verified", "verified="}
      ]
      |> Enum.filter(fn {_field, marker} -> output =~ marker end)
      |> Enum.map(&elem(&1, 0))
      |> MapSet.new()

    fixture_fields =
      Map.keys(fixture)
      |> MapSet.new()
      |> MapSet.union(
        fixture["evidence"]
        |> Enum.flat_map(&Map.keys/1)
        |> Enum.map(&"evidence.#{&1}")
        |> MapSet.new()
      )

    uncovered_fields =
      fixture_fields
      |> MapSet.difference(rendered_fields)
      |> MapSet.difference(@documented_omissions)

    IO.puts("rendered fields: #{rendered_fields |> MapSet.to_list() |> Enum.sort() |> Enum.join(", ")}")
    IO.puts("documented omissions: #{@documented_omissions |> MapSet.to_list() |> Enum.sort() |> Enum.join(", ")}")

    assert MapSet.size(uncovered_fields) == 0,
           "uncovered fields: #{uncovered_fields |> MapSet.to_list() |> Enum.sort() |> Enum.join(", ")}"
  end
end
