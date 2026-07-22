# l4dder-worker

Worker distribué ultra-léger pour **L4DDER**, le classement non-officiel de TR4KER.

TR4KER limite ses appels API **par IP**. Un seul scraper plafonne donc vite. Ce
worker permet à des volontaires de faire tourner une part du scraping **depuis leur
propre IP** : il demande une tranche de travail au serveur L4DDER, scrape les profils
concernés sur TR4KER, et renvoie les données brutes. Le serveur central reste seul
maître du calcul (seedtime, composite) — le worker est sans état et interchangeable.

- **Image Docker : ~2 Mo** (binaire Rust statique dans `scratch`)
- **RAM : <1 Mo** au repos (mesuré)
- Tourne aussi **sans Docker** (binaire unique)

## Comment ça marche

```
  ┌────────────┐   1. lease (demande N users)   ┌─────────────┐
  │  L4DDER    │ ─────────────────────────────► │   worker    │
  │  (serveur) │ ◄───────────────────────────── │ (chez toi)  │
  └────────────┘   3. submit (profils bruts)    └──────┬──────┘
                                                       │ 2. scrape depuis TA propre IP
                                                       ▼
                                                   ┌─────────┐
                                                   │ TR4KER  │
                                                   └─────────┘
```

Le worker ne calcule rien : il renvoie les profils horodatés, le serveur fait
l'intégration. Une tranche non rendue (worker coupé) retourne dans la file après
10 min. Chaque worker s'auto-régule sur les 429 de TR4KER (backoff AIMD).

## Configuration

Toute la config passe par variables d'environnement (voir [`.env.example`](.env.example)).

| Variable | Requis | Rôle |
|---|---|---|
| `LADDER_URL` | ✅ | URL du serveur L4DDER (ex `https://ladder.kheopsian.com`) |
| `WORKER_TOKEN` | ✅ | ton token worker (fourni par l'admin) |
| `TR4KER_USER` + `TR4KER_PASS` | (A) | tes identifiants TR4KER — re-login auto tous les ~30 j |
| `TR4KER_COOKIE` | (B) | ou colle ton cookie `TR4KER_session` — à renouveler à la main (~30 j) |
| `BATCH` | ⬜ | nb de users par lease (défaut 150) |
| `WORKER_LOG` | ⬜ | chemin d'un fichier où écrire les logs (indispensable en mode windowless Windows, voir plus bas) |

**Auth TR4KER : choisis (A) OU (B).** (A) est recommandé (le worker régénère son
cookie tout seul). (B) évite de stocker ton mot de passe mais expire au bout de ~30 j.

> ⚠️ Le worker scrape avec **ton** compte depuis **ton** IP : c'est ton quota TR4KER
> qui est utilisé. C'est voulu (chaque worker a son propre budget de rate-limit).

## Lancer

### Docker depuis l'image publiée (le plus simple — aucun build)

```bash
cp .env.example .env      # puis édite tes valeurs
docker run -d --name l4dder-worker --restart unless-stopped --env-file .env \
  ghcr.io/kheopsian/l4dder-worker:latest
```

L'image est publiée automatiquement (amd64 + arm64, tourne aussi sur un Pi).

### Docker en buildant toi-même

```bash
cp .env.example .env
docker build -t l4dder-worker .
docker run -d --name l4dder-worker --restart unless-stopped --env-file .env l4dder-worker
```

### Sans Docker (binaire natif)

```bash
cargo build --release
LADDER_URL=... WORKER_TOKEN=... TR4KER_USER=... TR4KER_PASS=... \
  ./target/release/l4dder-worker
```

### Windows — tourner en arrière-plan (sans fenêtre)

Le binaire **release** est compilé sans console (`windows_subsystem`) → il tourne en
tâche de fond, aucune fenêtre. Le build **debug** garde une console (pratique pour vérifier).

| Build | Fenêtre | Logs |
|---|---|---|
| `cargo run` (debug) | console visible | dans la console |
| `cargo build --release` | aucune (windowless) | via `WORKER_LOG` |

En release il n'y a **plus de console**, donc pour vérifier que ça tourne, définis
`WORKER_LOG` : le worker écrit alors ses logs dans ce fichier.

```powershell
$env:LADDER_URL="https://ladder.kheopsian.com"
$env:WORKER_TOKEN="<ton token>"
$env:TR4KER_USER="<compte>"; $env:TR4KER_PASS="<mdp>"
$env:WORKER_LOG="C:\l4dder.log"
.\target\release\l4dder-worker.exe
# suivre les logs en direct :
Get-Content C:\l4dder.log -Wait -Tail 20
```

## Confiance & données

Le serveur applique des garde-fous de sanité sur ce que renvoie un worker, mais la v1
suppose des workers **de confiance** (opérateurs connus). Ne distribue un token qu'à
des gens en qui tu as confiance.

## Licence

MIT — voir [LICENSE](LICENSE).
