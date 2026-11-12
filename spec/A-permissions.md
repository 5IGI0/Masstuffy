# Annexe A — Permissions

Elles ne font pas partie du système de fichiers, mais elles s'appuient sur le modèle : **les droits portent sur des collections, désignées par leur slug**.

## Modèle

Un **jeton** porte trois portées indépendantes :

- `read` : lire des records ;
- `write` : ajouter des records ;
- `delete` : supprimer.

Chaque portée est l'une des valeurs suivantes :

| Type | Paramètre | Accès accordé si… |
|---|---|---|
| `none` | — | jamais |
| `any` | — | toujours |
| `list` | `slug1,slug2,…` | le slug est dans la liste |
| `prefix` | `p` | le slug commence par `p` |

Un appel sans jeton connu prend les portées **anonymes** définies dans la configuration. Par défaut, elles valent toutes `any`.

## Remarques pour la réécriture

- Les droits portent sur le **slug**, qui est mutable. Si un renommage devient possible, il change silencieusement les droits. Il faut le savoir et l'accepter, ou bien attacher les droits à l'uuid.
- Les droits `prefix` encouragent des slugs hiérarchiques (`team-a/…`). Il peut être utile de l'officialiser (avec un séparateur réservé).
- Les droits manquants identifiés dans le legacy : création, suppression et énumération des collections.
