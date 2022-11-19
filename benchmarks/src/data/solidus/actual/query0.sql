SELECT `spree_products`.* FROM `spree_products` INNER JOIN `spree_variants` ON `spree_variants`.`is_master` = TRUE AND `spree_variants`.`product_id` = `spree_products`.`id` WHERE `spree_products`.`deleted_at` IS NULL AND EXISTS (SELECT `spree_prices`.* FROM `spree_prices` WHERE `spree_prices`.`deleted_at` IS NULL AND `spree_variants`.`id` = `spree_prices`.`variant_id`) AND (`spree_products`.available_on <= '2022-02-08 04:29:43.061313') AND (`spree_products`.discontinue_on IS NULL OR`spree_products`.discontinue_on >= '2022-02-08 04:29:43.062147') AND `spree_products`.`slug` = 'solidus-t-shirt' LIMIT 1; 