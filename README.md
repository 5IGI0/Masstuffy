# Masstuffy

Stockage d'objets **mono-machine** écrit en Rust, pensé pour ranger des **millions de petits objets** (pages web,
documents, réponses d'API…) de façon compacte, durable et rapide à relire. Chaque objet est un **record WARC**, le format
d'archivage du web (ISO 28500).

## Le problème

Stocker beaucoup de petits objets semble simple, mais les solutions habituelles cassent vite :

| Approche classique | Ce qui coince |
|---|---|
| **Un fichier par objet** | Des millions d'inodes, un système de fichiers lent à parcourir, à sauvegarder et à copier, et de l'espace perdu en fin de bloc pour chaque petit fichier. |
| **Une base de données (blobs)** | La base grossit sans limite, se compacte mal et devient le goulot d'étranglement. Les données sont enfermées dans un format propriétaire. |
| **Un stockage objet distribué** (S3, Ceph, MinIO…) | Un cluster à faire tourner et à surveiller, de la latence réseau, alors qu'une seule machine suffit pour le volume visé. |
| **Compresser chaque objet seul** | Un objet de quelques Ko compresse mal : le compresseur n'a pas d'historique sur lequel s'appuyer. |
| **Compresser de grosses archives d'un bloc** (`.tar.gz`, `.warc.gz` d'une traite) | Bon taux de compression, mais relire un seul objet oblige à décompresser tout ce qui le précède. |

Masstuffy vise le compromis qui manque : **le taux de compression d'une grosse archive, avec l'accès direct d'un
fichier par objet, sur une seule machine et sans service externe.**

## Comment Masstuffy le règle

### Peu de fichiers, beaucoup d'objets

Les records sont **concaténés** dans des **segments** de plusieurs Go, en ajout seul (append-only). Une collection de
dix millions d'objets tient dans une poignée de fichiers. Les copies, sauvegardes et `rsync` restent rapides.

### Un seul accès disque par lecture

Chaque record est repéré par un **localisateur** `(segment, offset, taille)`. Une lecture correspond à **une seule
lecture positionnelle** (`pread`) : pas de parcours, pas de décompression du voisinage. Les lectures ne prennent aucun
verrou et ne bloquent jamais les écritures.

### Bien compresser les petits objets

Chaque record est une **frame zstd indépendante**, compressée avec un **dictionnaire entraîné sur la collection**.
Le dictionnaire apporte l'historique commun qui manque à un petit objet seul. On obtient un taux proche d'une compression
de bout en bout, tout en gardant chaque objet lisible isolément.

Les objets d'une même collection se ressemblent (même site, même type de document), donc la collection est la bonne
unité pour un dictionnaire. Quand les données évoluent, on **entraîne un nouveau dictionnaire et on recompresse la
collection sans l'arrêter** : les écritures continuent pendant la reconstruction, et la nouvelle version ne devient
active qu'une fois complète.

### Autonome

Tout l'état tient dans **un répertoire de travail** : les segments, l'index de chaque collection (CDX), les
dictionnaires et un index de recherche embarqué ([redb](https://github.com/cberner/redb)). Pas de base de données à
installer, pas de cluster : un binaire et un dossier.

### Des données qui survivent au logiciel

Les segments sont des **fichiers WARC standards** (compressés en zstd). Un outil d'archivage web peut les relire sans
Masstuffy. Chaque collection commence par un record `warcinfo` qui la décrit. Les métadonnées de chaque objet voyagent
dans ses en-têtes WARC, et les octets d'origine sont conservés tels quels. WARC/1.0 et WARC/1.1 sont acceptés.

### Retrouver un objet

En plus du localisateur, l'index de recherche permet de retrouver un record :

- **par identifiant** (`WARC-Record-ID`) ;
- **par URL et date**, avec la sémantique « Wayback » : la version la plus proche de la date demandée ;
- **par motif d'URL** : un domaine avec ou sans ses sous-domaines, un hôte, un port, un chemin exact ou un préfixe de
  chemin ;
- en **parcours complet** d'une collection, ou en **export** vers un fichier WARC.

Cet index n'est qu'un cache : il se **reconstruit entièrement** à partir des index des collections, qui restent la
référence.

### Fiable en cas de crash

L'ordre des écritures est fixé et vérifié : d'abord les données, puis `fsync`, puis l'index, puis `fsync`, puis le
point de validation. Après un crash ou une coupure de courant, un objet est soit entièrement présent, soit absent.
Un objet dont l'écriture a été confirmée n'est jamais perdu. Les `fsync` sont regroupés entre écritures concurrentes
(group commit), pour garder un bon débit sans sacrifier la durabilité.

### Décharger le serveur

Le client peut **compresser et décompresser lui-même** : il récupère la configuration de la collection et son
dictionnaire (mis en cache, immuable), puis envoie des frames déjà compressées. Le serveur les vérifie et les range
sans recompresser. Une lecture peut aussi renvoyer la frame brute, que le client décompresse de son côté.

## Ce que Masstuffy n'est pas

- **Pas un système distribué** : une machine, un processus, un répertoire. La réplication et les sauvegardes se font au
  niveau des fichiers.
- **Pas fait pour modifier des objets** : un record est immuable. Le modifier, c'est en écrire un nouveau. La suppression
  est logique (l'objet devient invisible) et l'espace est récupéré lors de la reconstruction suivante.
- **Pas d'authentification** pour l'instant : l'API HTTP écoute en local par défaut et doit être placée derrière un
  proxy si elle est exposée.

## Démarrage rapide

```sh
# Lancer le serveur sur un nouveau dépôt (écoute sur 127.0.0.1:8470)
cargo run --release --bin masstuffy-server -- ./data --init

# Créer une collection (non compressée ; voir docs/http-api.md pour zstd)
curl -X POST localhost:8470/v1/collections -d '{"slug":"web"}'
```

Toutes les options du serveur se règlent aussi par variables d'environnement (`MASSTUFFY_WORKDIR`,
`MASSTUFFY_LISTEN`, `MASSTUFFY_INIT=1`…). La liste complète est dans `masstuffy-server --help`.

Un client Python est fourni, voir [`clients/python`](clients/python/README.md) :

```python
import masstuffy as m

with m.Client("http://127.0.0.1:8470") as client:
    col = client.create_collection("web")
    locs = col.put([m.RecordSpec("resource", b"hello", headers={"WARC-Target-URI": "http://a.example/"})])
    with col.get(locs[0]) as rec:
        print(rec.read())
```

## Organisation du dépôt

| Chemin | Contenu |
|---|---|
| [`spec/`](spec/README.md) | Spécification du concept et du format sur disque (en français) : la référence. |
| [`docs/architecture.md`](docs/architecture.md) | Carte des crates et graphe de dépendances. |
| [`docs/http-api.md`](docs/http-api.md) | Contrat de l'API HTTP v1. |
| `crates/warc`, `urlkey`, `zframe`, `cdx` | Formats : WARC, URL canonique, frames zstd et dictionnaires, index CDX. Réutilisables seuls. |
| `crates/durable`, `seglog`, `dictstore`, `collection` | Stockage : écritures durables, segments, dictionnaires, collections. |
| `crates/index`, `index-redb` | Index de recherche secondaire et son implémentation sur redb. |
| `crates/masstuffy` | Le moteur (asynchrone, tokio). |
| `crates/http` | L'API HTTP et le binaire `masstuffy-server`. |
| [`clients/python`](clients/python/README.md) | Client Python, basé sur [pywarc](https://github.com/5IGI0/pywarc). |

Le code est en Rust sûr (`unsafe` interdit dans toutes les crates). Chaque crate a une responsabilité unique et peut
servir hors de Masstuffy.

## Licence

AGPL-3.0-or-later.
