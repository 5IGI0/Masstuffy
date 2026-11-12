# Masstuffy — Spécification technique

Spécification du **concept** de Masstuffy et de son **système de fichiers**, écrite pour servir de base à une réécriture complète.

Ce qui est volontairement hors périmètre : l'interface HTTP, la CLI, le format de configuration réseau, l'authentification. Les permissions apparaissent seulement en annexe, parce qu'elles touchent au modèle (elles portent sur les collections).

Conventions utilisées dans ces documents :

- **[LEGACY]** décrit ce que fait l'implémentation actuelle, quand c'est utile pour comprendre une intention ou un piège.
- **[RECO]** est une recommandation pour la réécriture.
- Tout le reste décrit le concept : ce qui doit rester vrai quelle que soit l'implémentation.

## Sommaire

1. [Concepts et vocabulaire](01-concepts.md)
2. [Organisation sur disque](02-layout.md)
3. [Formats : WARC, frames compressées, index, dictionnaires](03-formats.md)
4. [Opérations](04-operations.md)
5. [Invariants, concurrence et reprise après crash](05-invariants.md)
6. [Points ouverts et recommandations pour la réécriture](06-reecriture.md)
7. [Annexe : permissions](A-permissions.md)

## En une page

Masstuffy est un **stockage d'objets mono-machine** qui range des millions de petits objets dans un petit nombre de gros fichiers, au lieu d'un fichier par objet.

Il répond à quatre besoins :

| Besoin | Réponse |
|---|---|
| Autonome, sans dépendance à un cluster | Tout tient dans un répertoire de travail (plus un index secondaire en base). |
| Ne pas gaspiller d'inodes | Les objets sont concaténés dans des **segments** de plusieurs Go. |
| Optimisé pour une seule machine | Accès direct par `(fichier, offset, taille)` et un seul `seek` par lecture. |
| Bien compresser les petits objets | Chaque objet est une **frame zstd indépendante**, compressée avec un **dictionnaire** entraîné sur la collection. |

Chaque objet est un **record WARC** (le format d'archivage du web, ISO 28500). Ce choix apporte trois choses : un format auto-descriptif avec des métadonnées extensibles (en-têtes), un format qu'on peut parcourir de façon séquentielle, et une compatibilité avec l'outillage d'archivage web.

Le modèle tient en quatre niveaux :

```
Repository ─┬─ Collection (manifest, dictionnaire courant)
            │     ├─ Segments : records.N[.dict].warc[.zstd]   ← données (append-only)
            │     └─ Index    : index.cdx                       ← localisation des records
            └─ Dictionary store (dictionnaires zstd partagés)

Index secondaire (base de données) : reconstruit à partir des index de collection,
                                     sert aux recherches par id, URL+date et motif d'URL.
```

Le principe central : **les données sont en append-only, l'index de collection sert de référence pour savoir quels records existent, et la base de données n'est qu'un cache de recherche qu'on peut reconstruire.**
