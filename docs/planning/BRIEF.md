# BRIEF — Janitor Prime (lane de PLANEJAMENTO, plan-only)

Voce e um Principal Systems Architect e especialista em Rust. Sua tarefa: escrever o PLANO
de implementacao do projeto "Janitor Prime". **PLAN ONLY: nada de codigo de implementacao
(sketches de assinatura/structs/trechos ilustrativos sao permitidos DENTRO do plano), nao
 rode cargo/npm, nao commite, nao crie PR.** Entregavel: o documento `docs/planning/PLAN.md`.

## Contexto do projeto

Janitor Prime e uma infraestrutura de alto desempenho em formato CLI/LSP que serve como
motor de refactoracao deterministica e memoria de longo prazo para agentes de IA autonomos.
Um unico executavel compilado estaticamente que funde 3 dominios:

1. **VFS (Virtual File System)**: overlay transacional para simular edicoes sem tocar o disco.
2. **AST Refactoring Engine**: interpretador de DSL de busca sobre tree-sitter, desenhado
   para resolver backtracking catastrofico via escopamento rigoroso.
3. **Hierarchical Persistent Memory (OptMem)**: log genetico append-only com compressao
   assincrona, para impedir que agentes consumidores sofram degradacao de contexto.

Inspiracoes (estude ambas):
- OptMem de Victor Taelin (https://github.com/VictorTaelin/OptMem): memoria permanente para
  agentes — prompt de ~426 tokens + script; log append-only de notas de 1 linha; arvore
  binaria de memorias; navegacao hierarquica #0-1, #2-3..., recall por regex, nap/compressao.
- refactree de lucasew (https://github.com/lucasew/refactree): CLI Go "rft" — ingest de code,
  pattern IR (pattern + replacement -> Match -> Edits), rft grep/rewrite estrutural, LSP, web viewer.

## Diretrizes globais (hard rules do produto)

- Concorrencia segura com zero-cost abstractions (Arc, RwLock).
- Erros tipados e propagaveis (thiserror). NENHUM unwrap() ou panic! em codigo de producao.
- Saida da CLI 100% JSON estruturado para parsing por agentes LLM consumidores.
- Base de codigo multi-linguagem em grande escala: otimizar alocacoes na travessia da arvore.

## Modulo 1 — VFS (src/vfs)

Clean Room Design, isolamento de estado:
1. Trait `ProjectFs` com metodos base: read_dir, read_file, stat.
2. Struct `Os`: proxy direto para std::fs.
3. Struct `Overlay`: `base_fs: Arc<dyn ProjectFs>` + `buffer: RwLock<HashMap<PathBuf, Vec<u8>>>`.
   - Invariante 1: read_file e stat consultam o buffer PRIMEIRO; cache-miss cai para base_fs.
   - Invariante 2 (Anti-Phantom): read_dir IGNORA o buffer e invoca APENAS o base_fs.
     Arquivos virtuais NUNCA aparecem na listagem.
   - Metodos de mutacao expostos: set, set_string, delete, get, get_string.

## Modulo 2 — Motor AST e Algebra Lisp (src/ast)

Avaliador de expressoes sobre o grafo do tree-sitter. AST interna (enum Rust) da DSL de busca:
- `Under(NodeType)`: pivo estrutural; restringe a maquina de estados aos limites deste no
  (previne varreduras exponenciais).
- `Seq(Vec<NodeId>)`: adjacencia estrita em-ordem.
- `Capture(Symbol)`: associa subarvore a um ID na tabela de simbolos.
- `Unify(Symbol, Symbol)`: resolve se duas subarvores capturadas sao isomorfas
  (equivalencia sintatica; base para abstracao de duplicidade).
- O parser tree-sitter DEVE ler os bytes primariamente do vfs::Overlay.

## Modulo 3 — Memoria Resiliente (src/memory)

1. Append-Only Immutable Log (LOG.txt): estrutura canonica de fatos.
2. ContextManager com camadas: Global (regras de lint/heuristicas AST genericas) e
   Local (`$PWD/.janitor/`, escopo do repo alvo).
3. Comandos: `wake(budget_tokens)` — carrega o log ate o orcamento; `note(pointer, content)`
   — SyntaxPointer forca referencia a no AST (node_id) ou hash de commit real (anti-alucinacao);
   `nap()` — rotina background que comprime o log numa arvore binaria de resumos, com
   `MemoryFlag::ArchitecturalLock` impedindo que fatos vitais sejam diluidos nas fusoes.

## Modulo 4 — CLI e Orquestracao (src/cli, src/orchestrator)

clap; subcomandos:
- `janitor init`: prepara VFS e inicializa memoria local.
- `janitor wake`: payload JSON com memorias global/local + status do linter.
- `janitor scan --rule "<dsl>"`: threads paralelas (rayon) buscando na AST via VFS inteiro;
  retorna coordenadas dos matches.
- `janitor apply --diff "<json_payload>"`: submete transformacoes ao buffer do Overlay;
  valida sintaxe resultante (re-parse) ANTES do flush para Os.

## Entregaveis exigidos NO PLANO (estrutura obrigatoria do PLAN.md)

1. **Design arquitetural detalhado** — em especial como o ciclo de vida dos ponteiros C do
   tree-sitter sera gerenciado com seguranca em Rust (ownership, lifetimes, Tree/Cursor/Node,
   relacao com o buffer do Overlay vivo, invalidacao de arvore apos edits).
2. **Cargo.toml completo** (dependecias reais verificadas: tree-sitter + grammars por
   linguagem, clap, rayon, serde, thiserror, tokio se justificado — pin de versoes reais;
   marque qualquer fato nao verificado com [VERIFY: source]).
3. **Fases de implementacao (F0..Fn)** estilo TDD: por fase, objetivo, tarefas bite-sized
   numeradas (arquivos exatos), gates de verificacao (cargo test/clippy -D warnings),
   commit boundary. F0 = scaffold + CI. Inclua os testes de invariante do VFS
   (anti-phantom: teste que FALHA se read_dir expuser arquivo fantasma; read-through:
   set -> read_file retorna o novo; fallback: sem set, le do disco; delete: read_file apos
   delete = miss/erro, mas read_dir nao muda).
4. **ADRs** para decisoes-chave (ex.: como representar NodeType multi-linguagem; se a DSL
   e s-expression ou outra sintaxe; como serializar Edits no apply; estrategia de grammars
   por linguagem; formato do LOG.txt; onde mora o indice de parse cache).
5. **Kill-points**: experimentos baratos e precoces que podem invalidar as apostas centrais
   (ex.: POC de parse incremental sobre overlay mutavel).
6. **Estimativa honesta** de esforco por fase e total. Sem otimismo de papel.
7. **MVP cut list**: o que fica fora do v0.1 (LSP? quais linguagens? watch mode?).
8. **Riscos + mitigacoes** (novos — nao copie esta lista; inclua os que voce descobrir).
9. **Open questions** triados: blocker vs noise.
10. **Escopo OSS futuro**: o que precisa estar limpo desde o dia 1 (secrets, licenca, credits
    as inspiracoes OptMem/refactree).

## Metodo de trabalho

- Pesquise na web (WebSearch/WebFetch) o que precisar: docs do tree-sitter (API Rust atual,
  tree_sitter::Tree/Cursor/Node, grammar crates oficiais por linguagem), OptMem, refactree,
  versoes reais no crates.io. Toda versao de crate no plano deve ser real e verificada;
  duvida = [VERIFY: source].
- Pode usar subagentes/teams para pesquisa paralela; MAS escreva o PLAN.md de forma
  INCREMENTAL (crie o arquivo cedo e va expandindo secao por secao) para sobreviver a
  qualquer interrupcao. Nao deixe o documento inteiro para o final.
- Nao commite. Nao rode cargo. Plan only.
- Idioma do plano: ingles (repo futuro open source). Commits/reports futuros serao em ingles.

## Output

Escreva `docs/planning/PLAN.md` (o plano completo estruturado acima). Ao final do processo,
imprima no stdout um resumo curto (<=40 linhas) das decisoes de arquitetura tomadas e das
open questions blockers.
