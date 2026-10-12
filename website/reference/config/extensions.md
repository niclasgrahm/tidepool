# extensions

tidepool's own settings in the model are `custom_extensions` entries with
`vendor_name: TIDEPOOL`. Their `data` is a JSON document in a string:

```yaml
# fragment
custom_extensions:
  - vendor_name: TIDEPOOL
    data: '{"kind": "derived"}'
```

Each part of the model can have one. An unknown key in `data` is an error.

## metrics

<!--@include: ../generated/config/metric-extension.md-->

## datasets

<!--@include: ../generated/config/dataset-extension.md-->

## fields

<!--@include: ../generated/config/field-extension.md-->

## relationships

<!--@include: ../generated/config/relationship-extension.md-->

## the model

<!--@include: ../generated/config/model-extension.md-->
